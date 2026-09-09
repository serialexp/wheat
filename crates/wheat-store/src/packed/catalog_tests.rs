use std::io::{Read, Seek, SeekFrom, Write};

use rusqlite::{Connection, params};

use super::catalog::*;
use super::segment::{CONTENT_NAMESPACE, SegmentRecord};

const LOCATOR_FILE: &str = "catalog-locator.sqlite3";

fn record(bytes: &'static [u8]) -> SegmentRecord<'static> {
    SegmentRecord {
        namespace: CONTENT_NAMESPACE,
        kind: 0,
        digest: *wheat_core::ContentId::of(bytes).as_bytes(),
        bytes,
    }
}

#[test]
fn catalog_reopens_without_derived_index_and_rejects_concurrent_open() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"packed payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert!(matches!(
            PackedCatalog::open(root.path(), crate::Durability::Durable),
            Err(CatalogError::Busy)
        ));
        store
            .put_records(&[SegmentRecord {
                namespace: CONTENT_NAMESPACE,
                kind: 0,
                digest,
                bytes,
            }])
            .unwrap();
        assert_eq!(
            store.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
            bytes
        );
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_eq!(
        reopened.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
        bytes
    );
}

#[test]
fn retry_after_segment_install_before_catalog_publication_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"retry payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    let record = SegmentRecord {
        namespace: CONTENT_NAMESPACE,
        kind: 0,
        digest,
        bytes,
    };
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[record]).unwrap();
        let catalogs = root.path().join("catalog");
        let published = std::fs::read_dir(&catalogs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(published).unwrap();
        std::fs::remove_file(root.path().join("catalog.head")).unwrap();
        // Simulate a crash after the immutable segment rename but before catalog/head publication.
        drop(store);
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert!(reopened.get(CONTENT_NAMESPACE, digest).unwrap().is_none());
    reopened.put_records(&[record]).unwrap();
    assert_eq!(
        reopened.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
        bytes
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("segments"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("catalog"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn catalog_rejects_corruption_and_ignores_unpublished_files() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store
            .put_records(&[SegmentRecord {
                namespace: CONTENT_NAMESPACE,
                kind: 0,
                digest,
                bytes,
            }])
            .unwrap();
    }
    std::fs::write(root.path().join("segments/orphan.segment"), b"incomplete").unwrap();
    assert!(PackedCatalog::open(root.path(), crate::Durability::Durable).is_ok());
    let catalog = std::fs::read_dir(root.path().join("catalog"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(catalog, b"corrupt").unwrap();
    assert!(PackedCatalog::open(root.path(), crate::Durability::Durable).is_err());
}

#[test]
fn matching_checkpoint_reopens_without_scanning_or_authenticating_old_storage() {
    let root = tempfile::tempdir().unwrap();
    let old = record(b"old payload");
    let new = record(b"new payload");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[old]).unwrap();
        store.put_records(&[new]).unwrap();
    }

    let old_segment = std::fs::read_dir(root.path().join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0-")
        })
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(old_segment)
        .unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
    // A malformed orphan would make a directory scan fail. Once catalog.head exists, it is not
    // authority and ordinary reopen must not enumerate catalog/ at all.
    std::fs::write(
        root.path().join("catalog/not-a-generation.catalog"),
        b"orphan",
    )
    .unwrap();

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_eq!(
        reopened
            .get(CONTENT_NAMESPACE, new.digest)
            .unwrap()
            .unwrap()
            .1,
        new.bytes
    );
    assert!(reopened.get(CONTENT_NAMESPACE, old.digest).is_err());
}

#[test]
fn missing_or_corrupt_locator_is_rebuilt() {
    for corrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let value = record(if corrupt {
            b"corrupt locator payload"
        } else {
            b"missing locator payload"
        });
        {
            let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
            store.put_records(&[value]).unwrap();
        }
        let locator = root.path().join(LOCATOR_FILE);
        if corrupt {
            std::fs::write(&locator, b"not sqlite").unwrap();
        } else {
            std::fs::remove_file(&locator).unwrap();
        }

        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert_eq!(
            reopened
                .get(CONTENT_NAMESPACE, value.digest)
                .unwrap()
                .unwrap()
                .1,
            value.bytes
        );
    }
}

#[test]
fn stale_checkpoint_rebuilds_after_catalog_publication_index_gap() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"first generation");
    let second = record(b"second generation");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[first]).unwrap();
        store.put_records(&[second]).unwrap();
    }

    let first_catalog = std::fs::read_dir(root.path().join("catalog"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0-")
        })
        .unwrap();
    let first_head = *blake3::hash(&std::fs::read(first_catalog).unwrap()).as_bytes();
    let mut connection = Connection::open(root.path().join(LOCATOR_FILE)).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "DELETE FROM locator WHERE digest = ?1",
            params![second.digest.as_slice()],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE checkpoint SET generation = 0, head_digest = ?1 WHERE singleton = 1",
            params![first_head.as_slice()],
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_eq!(
        reopened
            .get(CONTENT_NAMESPACE, second.digest)
            .unwrap()
            .unwrap()
            .1,
        second.bytes
    );
}

#[test]
fn incremental_publication_persists_locator_for_next_reopen() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"incremental first");
    let second = record(b"incremental second");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
        store.put_records(&[first]).unwrap();
    }
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
        store.put_records(&[second]).unwrap();
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
    for value in [first, second] {
        assert_eq!(
            reopened
                .get(CONTENT_NAMESPACE, value.digest)
                .unwrap()
                .unwrap()
                .1,
            value.bytes
        );
    }
}
