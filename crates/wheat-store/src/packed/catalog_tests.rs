use std::{fs, io::Write, os::unix::fs::MetadataExt};

use rusqlite::{Connection, params};

use super::catalog::*;
use super::segment::{CONTENT_NAMESPACE, MAX_SEGMENT_TAIL_BYTES, SegmentError, SegmentRecord};

const LOCATOR_FILE: &str = "catalog-locator.sqlite3";

fn record(bytes: &'static [u8]) -> SegmentRecord<'static> {
    SegmentRecord {
        namespace: CONTENT_NAMESPACE,
        kind: 0,
        digest: *wheat_core::ContentId::of(bytes).as_bytes(),
        bytes,
    }
}

fn assert_value(store: &mut PackedCatalog, value: SegmentRecord<'_>) {
    assert_eq!(
        store.get(CONTENT_NAMESPACE, value.digest).unwrap().unwrap(),
        (value.kind, value.bytes.to_vec())
    );
}

fn repack_options() -> CatalogRepackOptions {
    CatalogRepackOptions {
        max_records: 100,
        target_segment_bytes: 32,
    }
}

static NO_BASES: std::collections::BTreeMap<[u8; 32], [u8; 32]> = std::collections::BTreeMap::new();

fn refuse_loose(_: [u8; 32]) -> Result<Vec<u8>, CatalogError> {
    Err(CatalogError::Corrupt("test offers no loose metadata"))
}

/// Metadata input with no delta bases and no loose objects.
fn no_metadata(
    read_loose: &mut dyn FnMut([u8; 32]) -> Result<Vec<u8>, CatalogError>,
) -> MetadataRepackInput<'_> {
    MetadataRepackInput {
        bases: &NO_BASES,
        loose: &[],
        read_loose,
    }
}

fn repack(store: &mut PackedCatalog) -> Result<CatalogRepackReport, CatalogError> {
    let authority = store.physical_authority()?;
    store.repack_structural(
        repack_options(),
        no_content_generation(),
        None,
        authority,
        no_metadata(&mut refuse_loose),
    )
}

#[test]
fn repack_merges_segments_preserves_records_and_second_run_is_noop() {
    let root = tempfile::tempdir().unwrap();
    let values = [
        record(b"one authenticated value"),
        record(b"another authenticated value"),
    ];
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    for value in values {
        store.put_records(&[value]).unwrap();
    }
    assert_eq!(publication_names(root.path()).0.len(), 2);
    let options = CatalogRepackOptions {
        max_records: 100,
        target_segment_bytes: 1 << 20,
    };
    let authority = store.physical_authority().unwrap();
    let report = store
        .repack_structural(
            options,
            no_content_generation(),
            None,
            authority,
            no_metadata(&mut refuse_loose),
        )
        .unwrap();
    assert!(!report.no_op);
    assert_eq!(report.segments_before, 2);
    assert_eq!(report.segments_after, 1);
    assert_eq!(report.source_body_reads, 2);
    assert_eq!(report.max_resident_bodies, 1);
    assert_eq!(report.generation_after, report.generation_before + 1);
    for value in values {
        assert_value(&mut store, value);
    }
    let publications = publication_names(root.path());
    assert_eq!(publications.0.len(), 1);
    assert_eq!(publications.1.len(), 1);

    let authority = store.physical_authority().unwrap();
    let second = store
        .repack_structural(
            options,
            no_content_generation(),
            None,
            authority,
            no_metadata(&mut refuse_loose),
        )
        .unwrap();
    assert!(second.no_op);
    assert_eq!(second.generation_before, second.generation_after);
    drop(store);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    for value in values {
        assert_value(&mut reopened, value);
    }
}

#[test]
fn repack_rejects_fabricated_content_generation_authority() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"content must not disappear for a fabricated token");
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&[value]).unwrap();
    let generation = wheat_core::PhysicalContentGeneration {
        generation: 1,
        manifest: [0x42; 32],
    };
    assert!(matches!(
        store.repack_structural(
            repack_options(),
            generation,
            None,
            store.physical_authority().unwrap(),
            no_metadata(&mut refuse_loose),
        ),
        Err(CatalogError::Corrupt(_))
    ));
    assert_value(&mut store, value);
}

#[test]
fn repack_rejects_validated_inventory_missing_segment_content() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"coverage proof must include every segment content ID");
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&[value]).unwrap();
    let manifest = crate::ContentGenerationManifest {
        generation: 1,
        entries: Vec::new(),
        content_fingerprint: *blake3::hash(b"content").as_bytes(),
        manifest_digest: [0; 32],
    };
    let generation = wheat_core::PhysicalContentGeneration {
        generation: 1,
        manifest: [0x42; 32],
    };
    assert!(matches!(
        store.repack_structural(
            repack_options(),
            generation,
            Some(ValidatedContentGeneration::after_pack_verification(
                generation, &manifest,
            )),
            store.physical_authority().unwrap(),
            no_metadata(&mut refuse_loose),
        ),
        Err(CatalogError::Corrupt(_))
    ));
    assert_value(&mut store, value);
}

#[test]
fn ordinary_publication_after_repack_preserves_both_generations() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"first generation");
    let second = record(b"later ordinary publication");
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&[first]).unwrap();
    repack(&mut store).unwrap();
    store.put_records(&[second]).unwrap();
    drop(store);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, first);
    assert_value(&mut reopened, second);
}

#[test]
fn interrupted_repack_recovers_old_or_new_authority() {
    for failpoint in [RepackFailpoint::AfterIntent, RepackFailpoint::AfterHead] {
        let root = tempfile::tempdir().unwrap();
        let values = [record(b"recover one"), record(b"recover two")];
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&values).unwrap();
        let head_before = fs::read(root.path().join("catalog.head")).unwrap();
        store.test_set_repack_failpoint(failpoint);
        assert!(repack(&mut store).is_err());
        drop(store);

        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        for value in values {
            assert_value(&mut reopened, value);
        }
        assert!(!root.path().join("repack.intent").exists());
        assert_eq!(
            fs::read(root.path().join("catalog.head")).unwrap() == head_before,
            failpoint == RepackFailpoint::AfterIntent
        );
    }
}

#[test]
fn repack_inventory_limit_fails_before_publication() {
    let root = tempfile::tempdir().unwrap();
    let values = [record(b"limit one"), record(b"limit two")];
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&values).unwrap();
    let head_before = fs::read(root.path().join("catalog.head")).unwrap();
    let authority = store.physical_authority().unwrap();
    assert!(matches!(
        store.repack_structural(
            CatalogRepackOptions {
                max_records: 1,
                target_segment_bytes: 32,
            },
            no_content_generation(),
            None,
            authority,
            no_metadata(&mut refuse_loose),
        ),
        Err(CatalogError::RepackInventoryLimit { limit: 1 })
    ));
    assert_eq!(
        fs::read(root.path().join("catalog.head")).unwrap(),
        head_before
    );
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_catalog_snapshot(
    directory: &std::path::Path,
    snapshot: &wheat_core::PhysicalCatalogSnapshot,
) -> [u8; 32] {
    let encoded = wheat_core::encode_physical_catalog_snapshot(snapshot).unwrap();
    let digest = *blake3::hash(&encoded).as_bytes();
    fs::write(
        directory.join(format!(
            "{}-{}.catalog",
            snapshot.generation,
            lower_hex(&digest)
        )),
        encoded,
    )
    .unwrap();
    digest
}

/// The authoritative snapshot. Superseded snapshots are removed as soon as a
/// new head is durable, so exactly one catalog publication must remain.
fn head_snapshot(root: &std::path::Path) -> wheat_core::PhysicalCatalogSnapshot {
    let catalogs = publication_names(root).1;
    assert_eq!(catalogs.len(), 1, "{catalogs:?}");
    wheat_core::decode_physical_catalog_snapshot(
        &fs::read(root.join("catalog").join(&catalogs[0])).unwrap(),
    )
    .unwrap()
}

fn publication_names(root: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let names = |directory: &str| {
        let mut names = fs::read_dir(root.join(directory))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    (names("segments"), names("catalog"))
}

#[test]
fn deferred_many_tiny_batches_read_through_and_finalize_one_tail_on_close() {
    let root = tempfile::tempdir().unwrap();
    let values = [
        record(b"deferred one"),
        record(b"deferred two"),
        record(b"deferred three"),
    ];
    let mut store = PackedCatalog::open_with_test_threshold(
        root.path(),
        crate::Durability::Deferred,
        1024 * 1024,
    )
    .unwrap();
    for (index, value) in values.into_iter().enumerate() {
        store.put_records(&[value]).unwrap();
        assert_eq!(store.test_locator_rows(), (index + 1) as u64);
        assert_value(&mut store, value);
    }
    store.put_records(&values).unwrap();
    assert_eq!(store.test_locator_rows(), 3);
    assert_eq!(store.test_counts().unwrap(), (3, 2, 0));
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        0
    );
    store.close().unwrap();
    assert_eq!(
        fs::read_dir(root.path().join("segments")).unwrap().count(),
        1
    );
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        1
    );

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
    assert_eq!(reopened.test_locator_rows(), 3);
    assert_eq!(reopened.test_checkpoint_locator_rows().unwrap(), 3);
    for value in values {
        assert_value(&mut reopened, value);
    }
}

#[test]
fn deferred_rollover_publishes_full_packs_and_close_publishes_final_tail() {
    let root = tempfile::tempdir().unwrap();
    let values = [
        record(b"roll one"),
        record(b"roll two"),
        record(b"roll three"),
    ];
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Deferred, 1)
            .unwrap();
    for value in values {
        store.put_records(&[value]).unwrap();
        assert_value(&mut store, value);
    }
    let first_pack = fs::read_dir(root.path().join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let first_pack_inode = fs::metadata(&first_pack).unwrap().ino();
    let first_pack_bytes = fs::read(&first_pack).unwrap();
    // Each following append rolls the preceding pack; the last remains mutable.
    assert_eq!(head_snapshot(root.path()).segments.len(), 2);
    store.close().unwrap();
    assert_eq!(
        fs::read_dir(root.path().join("segments")).unwrap().count(),
        3
    );
    assert_eq!(head_snapshot(root.path()).segments.len(), 3);
    assert_eq!(fs::metadata(&first_pack).unwrap().ino(), first_pack_inode);
    assert_eq!(fs::read(&first_pack).unwrap(), first_pack_bytes);
}

#[test]
fn compressed_records_use_stored_bytes_for_rollover_and_logical_read_limits() {
    let root = tempfile::tempdir().unwrap();
    let first_bytes = vec![b'a'; 8 * 1024];
    let second_bytes = vec![b'b'; 8 * 1024];
    let values = [
        SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: *wheat_core::ContentId::of(&first_bytes).as_bytes(),
            bytes: &first_bytes,
        },
        SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: *wheat_core::ContentId::of(&second_bytes).as_bytes(),
            bytes: &second_bytes,
        },
    ];
    let stored = crate::codec::prepare(&first_bytes).unwrap().bytes.len() as u64;
    assert!(stored < first_bytes.len() as u64);

    let mut store = PackedCatalog::open_with_test_limits(
        root.path(),
        crate::Durability::Durable,
        u64::MAX,
        100,
        stored * 2,
    )
    .unwrap();
    store.put_records(&values).unwrap();
    assert_eq!(store.test_counts().unwrap().0, 2);
    assert!(matches!(
        store.get_bounded(CONTENT_NAMESPACE, values[0].digest, first_bytes.len() - 1),
        Err(CatalogError::Segment(SegmentError::ReadLimit { minimum }))
            if minimum == first_bytes.len()
    ));
    assert_value(&mut store, values[0]);
    assert_value(&mut store, values[1]);
}

#[test]
fn deferred_duplicate_is_idempotent_and_conflict_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let original = record(b"same digest authority");
    let mut conflicting = original;
    conflicting.bytes = b"different bytes";
    let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
    store.put_records(&[original, original]).unwrap();
    assert_eq!(store.test_counts().unwrap().0, 1);
    assert!(matches!(
        store.put_records(&[conflicting]),
        Err(CatalogError::Corrupt(
            "existing key differs from proposed bytes"
        ))
    ));
}

#[test]
fn active_appends_reopen_without_immutable_catalog_generations_and_lock_excludes_writers() {
    let root = tempfile::tempdir().unwrap();
    let values = [record(b"first"), record(b"second"), record(b"third")];
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert!(matches!(
            PackedCatalog::open(root.path(), crate::Durability::Durable),
            Err(CatalogError::Busy)
        ));
        for value in values {
            store.put_records(&[value]).unwrap();
        }
        assert_eq!(store.test_counts().unwrap(), (3, 1, 0));
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    for value in values {
        assert_value(&mut reopened, value);
    }
    assert_eq!(reopened.test_counts().unwrap(), (3, 1, 0));
}

#[test]
fn active_locator_append_does_not_rewrite_existing_rows() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"stable locator row");
    let second = record(b"new locator row");
    let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    store.put_records(&[first]).unwrap();
    store.test_reject_locator_rewrite(first.digest).unwrap();
    store.put_records(&[second]).unwrap();
    assert_value(&mut store, first);
    assert_value(&mut store, second);
}

#[test]
fn immutable_only_store_is_preserved_and_gains_an_active_pack() {
    let root = tempfile::tempdir().unwrap();
    let old = record(b"immutable only");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[old]).unwrap();
        assert_eq!(store.test_counts().unwrap(), (1, 2, 1));
    }
    fs::remove_file(root.path().join("active.head")).unwrap();
    fs::remove_file(root.path().join("active.segment")).unwrap();
    fs::remove_file(root.path().join(LOCATOR_FILE)).unwrap();

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, old);
    assert!(root.path().join("active.head").exists());
    assert!(root.path().join("active.segment").exists());
}

#[test]
fn missing_corrupt_stale_or_old_schema_locator_rebuilds_with_active_state() {
    for mode in 0..4 {
        let root = tempfile::tempdir().unwrap();
        let value = record(match mode {
            0 => b"missing",
            1 => b"corrupt",
            2 => b"stale",
            _ => b"old schema",
        });
        {
            let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
            store.put_records(&[value]).unwrap();
        }
        let locator = root.path().join(LOCATOR_FILE);
        match mode {
            0 => fs::remove_file(&locator).unwrap(),
            1 => fs::write(&locator, b"not sqlite").unwrap(),
            2 => {
                let connection = Connection::open(&locator).unwrap();
                connection.execute("DELETE FROM locator", []).unwrap();
                connection
                    .execute("UPDATE checkpoint SET active_generation=99", [])
                    .unwrap();
            }
            _ => {
                let connection = Connection::open(&locator).unwrap();
                connection.pragma_update(None, "user_version", 1).unwrap();
            }
        }
        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert_value(&mut reopened, value);
        assert_eq!(reopened.test_counts().unwrap(), (1, 1, 0));
    }
}

#[test]
fn same_checkpoint_missing_locator_row_is_rebuilt_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"deleted locator row");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[value]).unwrap();
    }
    let connection = Connection::open(root.path().join(LOCATOR_FILE)).unwrap();
    connection
        .execute(
            "DELETE FROM locator WHERE digest=?1",
            [value.digest.as_slice()],
        )
        .unwrap();
    drop(connection);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_eq!(reopened.test_sealed_authentications(), 1);
    assert_value(&mut reopened, value);
    assert_value(&mut reopened, value);
    assert!(
        reopened
            .get(CONTENT_NAMESPACE, [0x5a; 32])
            .unwrap()
            .is_none()
    );
    assert_eq!(reopened.test_sealed_authentications(), 1);
    assert_eq!(reopened.test_counts().unwrap().0, 1);
}

#[test]
fn journal_with_old_head_restores_the_previous_active_commit() {
    let root = tempfile::tempdir().unwrap();
    let old = record(b"published");
    let attempted = record(b"uncommitted");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[old]).unwrap();
        store.test_interrupt_append(&[attempted], false).unwrap();
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, old);
    assert!(
        reopened
            .get(CONTENT_NAMESPACE, attempted.digest)
            .unwrap()
            .is_none()
    );
    assert!(!root.path().join("active.append").exists());
}

#[test]
fn journal_with_intended_head_validates_append_and_repairs_locator() {
    let root = tempfile::tempdir().unwrap();
    let old = record(b"published");
    let appended = record(b"committed before sqlite");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[old]).unwrap();
        store.test_interrupt_append(&[appended], true).unwrap();
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, old);
    assert_value(&mut reopened, appended);
    assert_eq!(reopened.test_counts().unwrap(), (2, 1, 0));
    assert!(!root.path().join("active.append").exists());
}

#[test]
fn corrupt_active_control_file_is_rejected_before_locator_is_trusted() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"authority");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[value]).unwrap();
    }
    fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("active.head"))
        .unwrap()
        .write_all(b"corruption")
        .unwrap();
    assert!(PackedCatalog::open(root.path(), crate::Durability::Durable).is_err());
}

#[test]
fn durable_post_mutation_errors_poison_and_recover_without_retry_ambiguity() {
    for failpoint in [
        AppendFailpoint::MutationApplied,
        AppendFailpoint::HeadPublished,
        AppendFailpoint::JournalRemoved,
    ] {
        let root = tempfile::tempdir().unwrap();
        let attempted = record(b"durable attempted append");
        let later = record(b"durable later append");
        {
            let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
            store.test_set_append_failpoint(failpoint);
            assert!(store.put_records(&[attempted]).is_err());
            assert!(matches!(
                store.put_records(&[later]),
                Err(CatalogError::Poisoned)
            ));
        }
        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        let present = reopened
            .get(CONTENT_NAMESPACE, attempted.digest)
            .unwrap()
            .is_some();
        assert_eq!(present, failpoint != AppendFailpoint::MutationApplied);
        reopened.put_records(&[attempted]).unwrap();
        assert_value(&mut reopened, attempted);
    }
}

#[test]
fn deferred_index_failure_cannot_publish_unindexed_object() {
    let root = tempfile::tempdir().unwrap();
    let attempted = record(b"deferred unindexed append");
    let later = record(b"deferred later append");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
        store.test_set_deferred_failpoint(DeferredFailpoint::IndexCommit);
        assert!(store.put_records(&[attempted]).is_err());
        assert!(matches!(
            store.put_records(&[later]),
            Err(CatalogError::Poisoned)
        ));
        assert!(matches!(store.finalize_bulk(), Err(CatalogError::Poisoned)));
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
    assert!(
        reopened
            .get(CONTENT_NAMESPACE, attempted.digest)
            .unwrap()
            .is_none()
    );
    reopened.put_records(&[attempted]).unwrap();
    reopened.close().unwrap();
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, attempted);
}

#[test]
fn deferred_post_authority_error_poisoned_handle_recovers_published_object() {
    let root = tempfile::tempdir().unwrap();
    let attempted = record(b"deferred published append");
    let later = record(b"deferred must reopen");
    {
        let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
        store.put_records(&[attempted]).unwrap();
        store.test_set_deferred_failpoint(DeferredFailpoint::AfterCatalogHead);
        assert!(store.finalize_bulk().is_err());
        assert!(matches!(
            store.put_records(&[later]),
            Err(CatalogError::Poisoned)
        ));
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, attempted);
    assert!(
        reopened
            .get(CONTENT_NAMESPACE, later.digest)
            .unwrap()
            .is_none()
    );
}

#[test]
fn seal_errors_after_authority_publication_poison_writer_and_recover_on_reopen() {
    for failpoint in [
        SealFailpoint::SqliteAfterHead,
        SealFailpoint::DuringRetirement,
    ] {
        let root = tempfile::tempdir().unwrap();
        let sealed = record(b"published before returned seal error");
        let later = record(b"must not mutate published inode");
        {
            let mut store =
                PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                    .unwrap();
            store.test_set_seal_failpoint(failpoint);
            assert!(store.put_records(&[sealed]).is_err());
            // Reads remain pinned to authenticated active/sealed readers even
            // though this writer may no longer append until recovery.
            assert_value(&mut store, sealed);
            assert!(matches!(
                store.put_records(&[later]),
                Err(CatalogError::Poisoned)
            ));
        }
        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert_value(&mut reopened, sealed);
        assert!(
            reopened
                .get(CONTENT_NAMESPACE, later.digest)
                .unwrap()
                .is_none()
        );
        reopened.put_records(&[later]).unwrap();
        assert_value(&mut reopened, later);
    }
}

#[test]
fn empty_active_without_head_recovers_but_nonempty_unpublished_active_is_rejected() {
    let fresh = tempfile::tempdir().unwrap();
    {
        let _store = PackedCatalog::open(fresh.path(), crate::Durability::Durable).unwrap();
    }
    fs::remove_file(fresh.path().join("active.head")).unwrap();
    let _reopened = PackedCatalog::open(fresh.path(), crate::Durability::Durable).unwrap();

    let unpublished = tempfile::tempdir().unwrap();
    let value = record(b"not published by a head");
    {
        let mut store =
            PackedCatalog::open(unpublished.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[value]).unwrap();
    }
    fs::remove_file(unpublished.path().join("active.head")).unwrap();
    assert!(PackedCatalog::open(unpublished.path(), crate::Durability::Durable).is_err());
}

#[test]
fn valid_sqlite_semantic_corruption_is_rebuilt_before_read_allocation() {
    for column in ["kind", "offset", "stored_length", "logical_length", "path"] {
        let root = tempfile::tempdir().unwrap();
        let value = record(b"authoritative payload");
        {
            let mut store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
            store.put_records(&[value]).unwrap();
        }
        let connection = Connection::open(root.path().join(LOCATOR_FILE)).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        match column {
            "kind" => connection
                .execute("UPDATE locator SET kind=42", [])
                .unwrap(),
            "offset" => connection
                .execute("UPDATE locator SET offset=999999", [])
                .unwrap(),
            "stored_length" => connection
                .execute("UPDATE locator SET stored_length=9223372036854775807", [])
                .unwrap(),
            "logical_length" => connection
                .execute("UPDATE locator SET logical_length=9223372036854775807", [])
                .unwrap(),
            "path" => connection
                .execute("UPDATE physical_source SET path='../arbitrary'", [])
                .unwrap(),
            _ => unreachable!(),
        };
        drop(connection);
        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        // The checkpoint still matches exactly; get must distrust the remaining
        // valid SQLite and rebuild from authenticated authority once.
        assert_value(&mut reopened, value);
    }
}

#[test]
fn head_discovery_ignores_malformed_and_orphan_higher_generations() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"valid discovered head");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[value]).unwrap();
    }
    fs::remove_file(root.path().join("catalog.head")).unwrap();
    let catalogs = root.path().join("catalog");
    fs::write(catalogs.join("garbage.catalog"), b"garbage").unwrap();
    fs::write(catalogs.join("01-deadbeef.catalog"), b"garbage").unwrap();
    fs::write(
        catalogs.join("9-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.catalog"),
        b"garbage",
    )
    .unwrap();
    // A well-formed snapshot whose segment is missing must not win discovery.
    write_catalog_snapshot(
        &catalogs,
        &wheat_core::PhysicalCatalogSnapshot {
            generation: 8,
            segments: vec![wheat_core::PhysicalSegmentDescriptor {
                generation: 8,
                sequence: 0,
                segment: [9; 32],
                stored_length: 123,
            }],
            content: no_content_generation(),
        },
    );

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, value);
}

#[test]
fn reopen_removes_only_authenticated_canonical_publications_unreachable_from_head() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"authoritative sealed publication");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[value]).unwrap();
    }
    let authoritative = publication_names(root.path());
    assert_eq!(authoritative.0.len(), 1);
    assert_eq!(authoritative.1.len(), 1);

    let segments = root.path().join("segments");
    let catalogs = root.path().join("catalog");
    let segment_bytes = fs::read(segments.join(&authoritative.0[0])).unwrap();
    let catalog_bytes = fs::read(catalogs.join(&authoritative.1[0])).unwrap();
    let authority = wheat_core::decode_physical_catalog_snapshot(&catalog_bytes).unwrap();
    let sealed = authority.segments[0].clone();
    let segment_hex = lower_hex(&sealed.segment);
    for generation in [authority.generation + 1, authority.generation + 2] {
        let orphan_segment = format!("{generation}-0-{segment_hex}.segment");
        fs::write(segments.join(&orphan_segment), &segment_bytes).unwrap();
        write_catalog_snapshot(
            &catalogs,
            &wheat_core::PhysicalCatalogSnapshot {
                generation,
                segments: vec![wheat_core::PhysicalSegmentDescriptor {
                    generation,
                    ..sealed.clone()
                }],
                content: authority.content,
            },
        );
    }
    // Fail closed for names or contents which cannot be proven to be a
    // canonical, authenticated immutable publication.
    fs::write(segments.join("01-deadbeef.segment"), b"preserve").unwrap();
    let two_part_segment = format!("99-{}.segment", "0".repeat(64));
    fs::write(segments.join(&two_part_segment), b"preserve").unwrap();
    let malformed_segment = format!("99-0-{}.segment", "0".repeat(64));
    fs::write(segments.join(&malformed_segment), b"preserve").unwrap();
    fs::write(catalogs.join("garbage.catalog"), b"preserve").unwrap();
    let malformed_catalog = format!("99-{}.catalog", "0".repeat(64));
    fs::write(catalogs.join(&malformed_catalog), b"preserve").unwrap();

    for _ in 0..2 {
        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert_value(&mut reopened, value);
        drop(reopened);
        let names = publication_names(root.path());
        assert_eq!(
            names.0,
            [
                authoritative.0[0].clone(),
                "01-deadbeef.segment".into(),
                malformed_segment.clone(),
                two_part_segment.clone(),
            ]
        );
        assert_eq!(
            names.1,
            [
                authoritative.1[0].clone(),
                malformed_catalog.clone(),
                "garbage.catalog".into(),
            ]
        );
        assert!(root.path().join("catalog.head").exists());
        assert!(root.path().join("active.segment").exists());
    }
}

#[test]
fn no_head_initial_state_removes_authenticated_orphan_segment_only() {
    let donor = tempfile::tempdir().unwrap();
    let value = record(b"orphan segment without catalog publication");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(donor.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[value]).unwrap();
    }
    let donor_segment = fs::read_dir(donor.path().join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();

    let root = tempfile::tempdir().unwrap();
    {
        let _store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    }
    let orphan_path = root.path().join("segments").join(donor_segment.file_name());
    fs::copy(donor_segment.path(), &orphan_path).unwrap();
    assert!(orphan_path.exists());

    let _reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert!(!orphan_path.exists());
    assert!(root.path().join("active.segment").exists());
    assert!(root.path().join("catalog.head").exists());
}

#[test]
fn authoritative_publication_survives_cleanup_after_head_commit() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"first authoritative generation");
    let second = record(b"second authoritative generation");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[first]).unwrap();
        store.put_records(&[second]).unwrap();
    }
    let authoritative = publication_names(root.path());
    assert_eq!(authoritative.0.len(), 2);
    // Only the head snapshot survives: each seal removes the one it supersedes.
    assert_eq!(authoritative.1.len(), 1);
    assert!(authoritative.1[0].starts_with("1-"));

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, first);
    assert_value(&mut reopened, second);
    drop(reopened);
    assert_eq!(publication_names(root.path()), authoritative);
}

#[test]
fn head_discovery_rejects_duplicate_valid_latest_generation() {
    let root = tempfile::tempdir().unwrap();
    let catalogs = root.path().join("catalog");
    fs::create_dir_all(&catalogs).unwrap();
    for manifest in [[1; 32], [2; 32]] {
        write_catalog_snapshot(
            &catalogs,
            &wheat_core::PhysicalCatalogSnapshot {
                generation: 0,
                segments: Vec::new(),
                content: wheat_core::PhysicalContentGeneration {
                    generation: 1,
                    manifest,
                },
            },
        );
    }
    assert!(matches!(
        PackedCatalog::open(root.path(), crate::Durability::Durable),
        Err(CatalogError::Corrupt(
            "multiple valid catalog heads at latest generation"
        ))
    ));
}

#[test]
fn oversized_active_journal_is_rejected_before_allocation() {
    let root = tempfile::tempdir().unwrap();
    {
        let _store = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    }
    let journal = fs::File::create(root.path().join("active.append")).unwrap();
    journal.set_len(MAX_SEGMENT_TAIL_BYTES + 1024).unwrap();
    assert!(matches!(
        PackedCatalog::open(root.path(), crate::Durability::Durable),
        Err(CatalogError::Corrupt("active journal exceeds tail limit"))
    ));
}

#[test]
fn duplicate_heavy_batch_does_not_false_preseal() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"one projected key");
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1_000)
            .unwrap();
    let duplicates = vec![value; 100];
    store.put_records(&duplicates).unwrap();
    assert_eq!(store.test_counts().unwrap(), (1, 1, 0));
}

#[test]
fn one_durable_batch_rolls_at_target_count_and_payload_seams() {
    for (target, max_entries, max_payload) in
        [(1, 100, 100), (u64::MAX, 2, 100), (u64::MAX, 100, 3)]
    {
        let root = tempfile::tempdir().unwrap();
        let values = [record(b"a"), record(b"bb"), record(b"c"), record(b"dd")];
        let mut store = PackedCatalog::open_with_test_limits(
            root.path(),
            crate::Durability::Durable,
            target,
            max_entries,
            max_payload,
        )
        .unwrap();
        store.put_records(&values).unwrap();
        for value in values {
            assert_value(&mut store, value);
        }
        assert!(fs::read_dir(root.path().join("segments")).unwrap().count() >= 1);
    }
}

#[test]
fn impossible_record_leaves_prior_active_state_exact_and_usable() {
    let root = tempfile::tempdir().unwrap();
    let prior = record(b"old");
    let impossible = record(b"12345");
    let later = record(b"new");
    let mut store = PackedCatalog::open_with_test_limits(
        root.path(),
        crate::Durability::Durable,
        u64::MAX,
        10,
        4,
    )
    .unwrap();
    store.put_records(&[prior]).unwrap();
    let old_active = fs::read(root.path().join("active.segment")).unwrap();
    let old_head = fs::read(root.path().join("active.head")).unwrap();
    let old_counts = store.test_counts().unwrap();

    assert!(matches!(
        store.put_records(&[later, impossible]),
        Err(CatalogError::Segment(SegmentError::TooLarge))
    ));
    assert_eq!(
        fs::read(root.path().join("active.segment")).unwrap(),
        old_active
    );
    assert_eq!(fs::read(root.path().join("active.head")).unwrap(), old_head);
    assert_eq!(store.test_counts().unwrap(), old_counts);
    assert_value(&mut store, prior);
    store.put_records(&[later]).unwrap();
    assert_value(&mut store, later);
}

#[test]
fn deferred_close_failure_does_not_publish_unfinalized_bulk() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"unfinalized");
    let mut store = PackedCatalog::open(root.path(), crate::Durability::Deferred).unwrap();
    store.put_records(&[value]).unwrap();
    store.test_set_close_failpoint(CloseFailpoint::BeforeFinalize);
    assert!(matches!(
        store.close(),
        Err(CatalogError::Corrupt(
            "test failure before deferred bulk finalization"
        ))
    ));
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        0
    );
    let segments: Vec<_> = fs::read_dir(root.path().join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(segments.len(), 1);
    assert!(segments[0].starts_with("bulk-"));
}

#[test]
fn threshold_seal_publishes_once_repoints_one_source_and_bounds_counts() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"seal me");
    let second = record(b"remain active");
    {
        let mut store =
            PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
                .unwrap();
        store.put_records(&[first]).unwrap();
        assert_eq!(store.test_counts().unwrap(), (1, 2, 1));
        store.put_records(&[first]).unwrap();
        assert_eq!(store.test_counts().unwrap(), (1, 2, 1));
    }
    {
        let connection = Connection::open(root.path().join(LOCATOR_FILE)).unwrap();
        let path: String = connection
            .query_row(
                "SELECT s.path FROM locator l JOIN physical_source s ON s.id=l.source_id WHERE l.digest=?1",
                params![first.digest.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(path.starts_with("segments/"));
    }
    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    reopened.put_records(&[second]).unwrap();
    assert_eq!(reopened.test_sealed_authentications(), 1);
    assert_value(&mut reopened, first);
    assert_eq!(reopened.test_sealed_authentications(), 1);
    assert_value(&mut reopened, first);
    assert_eq!(reopened.test_sealed_authentications(), 1);
    assert_value(&mut reopened, second);
    assert_eq!(reopened.test_counts().unwrap(), (2, 2, 1));
    assert_eq!(
        fs::read_dir(root.path().join("segments")).unwrap().count(),
        1
    );
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        1
    );
}

/// `count` canonical content sets, newest (largest) first; set `k + 1` is set
/// `k` minus one member, so each older set is a small delta of its successor.
fn content_set_history(count: usize) -> Vec<([u8; 32], Vec<u8>)> {
    (0..count)
        .map(|age| {
            let mut members = (0..200 - age)
                .map(|index| wheat_core::ContentId::of(format!("member {index}").as_bytes()))
                .collect::<Vec<_>>();
            members.sort_unstable();
            let bytes = wheat_core::content_set_snapshot_bytes(&members);
            (
                *wheat_core::ObjectId::of_canonical_bytes(&bytes).as_bytes(),
                bytes,
            )
        })
        .collect()
}

fn metadata_records(history: &[([u8; 32], Vec<u8>)]) -> Vec<SegmentRecord<'_>> {
    let kind = wheat_core::SegmentIndexEntry::canonical_kind_code(
        wheat_core::CanonicalObjectKind::ContentSetSnapshot,
    );
    history
        .iter()
        .map(|(digest, bytes)| SegmentRecord {
            namespace: super::segment::METADATA_NAMESPACE,
            kind,
            digest: *digest,
            bytes,
        })
        .collect()
}

/// Every older set deltas against the next newer one.
fn history_bases(
    history: &[([u8; 32], Vec<u8>)],
) -> std::collections::BTreeMap<[u8; 32], [u8; 32]> {
    history
        .windows(2)
        .map(|pair| (pair[1].0, pair[0].0))
        .collect()
}

fn metadata_catalog(root: &std::path::Path, history: &[([u8; 32], Vec<u8>)]) -> PackedCatalog {
    let mut store =
        PackedCatalog::open_with_test_threshold(root, crate::Durability::Durable, 4).unwrap();
    for record in metadata_records(history) {
        store.put_records(&[record]).unwrap();
    }
    store
}

fn repack_history(
    store: &mut PackedCatalog,
    bases: &std::collections::BTreeMap<[u8; 32], [u8; 32]>,
    target_segment_bytes: u64,
) -> Result<CatalogRepackReport, CatalogError> {
    let authority = store.physical_authority()?;
    store.repack_structural(
        CatalogRepackOptions {
            max_records: 1_000,
            target_segment_bytes,
        },
        no_content_generation(),
        None,
        authority,
        MetadataRepackInput {
            bases,
            loose: &[],
            read_loose: &mut refuse_loose,
        },
    )
}

fn assert_history(store: &mut PackedCatalog, history: &[([u8; 32], Vec<u8>)]) {
    for record in metadata_records(history) {
        assert_eq!(
            store
                .get(super::segment::METADATA_NAMESPACE, record.digest)
                .unwrap()
                .unwrap(),
            (record.kind, record.bytes.to_vec())
        );
    }
}

#[test]
fn a_full_segment_cuts_the_chain_and_stores_the_next_record_whole() {
    let history = content_set_history(12);
    let bases = history_bases(&history);

    let uncut_root = tempfile::tempdir().unwrap();
    let mut uncut = metadata_catalog(uncut_root.path(), &history);
    let whole_chain = repack_history(&mut uncut, &bases, 1 << 20).unwrap();
    assert_eq!(whole_chain.segments_after, 1);
    assert_eq!(whole_chain.delta_records, 11);
    assert_eq!(whole_chain.max_delta_depth, 11);

    // Room for one whole set and a delta or two (each patch here is a few
    // dozen bytes): the chain must be cut, and each cut costs one delta.
    let root = tempfile::tempdir().unwrap();
    let mut store = metadata_catalog(root.path(), &history);
    let target = history[0].1.len() as u64 + 48;
    let report = repack_history(&mut store, &bases, target).unwrap();
    assert!(report.segments_after > 1, "{report:?}");
    assert_eq!(
        report.delta_records,
        whole_chain.delta_records + 1 - report.segments_after as u64,
        "every segment after the first starts whole: {report:?}"
    );
    assert_history(&mut store, &history);
    drop(store);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_history(&mut reopened, &history);
    let second = repack_history(&mut reopened, &bases, target).unwrap();
    assert!(second.no_op, "{second:?}");
    assert_eq!(second.delta_records, report.delta_records);
}

#[test]
fn interrupted_delta_repack_recovers_old_or_new_authority() {
    let history = content_set_history(6);
    let bases = history_bases(&history);
    for failpoint in [RepackFailpoint::AfterIntent, RepackFailpoint::AfterHead] {
        let root = tempfile::tempdir().unwrap();
        let mut store = metadata_catalog(root.path(), &history);
        let head_before = fs::read(root.path().join("catalog.head")).unwrap();
        store.test_set_repack_failpoint(failpoint);
        assert!(repack_history(&mut store, &bases, 1 << 20).is_err());
        drop(store);

        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        assert_history(&mut reopened, &history);
        assert!(!root.path().join("repack.intent").exists());
        assert_eq!(
            fs::read(root.path().join("catalog.head")).unwrap() == head_before,
            failpoint == RepackFailpoint::AfterIntent
        );
        // Whichever authority survived, finishing the repack converges on
        // the same delta layout.
        let finished = repack_history(&mut reopened, &bases, 1 << 20).unwrap();
        assert_eq!(finished.no_op, failpoint == RepackFailpoint::AfterHead);
        assert_eq!(finished.delta_records, 5);
        assert_history(&mut reopened, &history);
    }
}
