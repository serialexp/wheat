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

fn repack(store: &mut PackedCatalog) -> Result<CatalogRepackReport, CatalogError> {
    let authority = store.physical_authority()?;
    store.repack_structural(repack_options(), legacy_content_generation(), authority)
}

#[test]
fn repack_converts_v1_to_v2_preserves_records_and_second_run_is_noop() {
    let root = tempfile::tempdir().unwrap();
    let values = [
        record(b"one authenticated value"),
        record(b"another authenticated value"),
    ];
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&values).unwrap();
    let report = repack(&mut store).unwrap();
    assert!(!report.no_op);
    assert_eq!(report.source_body_reads, 2);
    assert_eq!(report.max_resident_bodies, 1);
    assert_eq!(report.generation_after, report.generation_before + 1);
    assert!(store.test_is_v2());
    for value in values {
        assert_value(&mut store, value);
    }
    let publications = publication_names(root.path());
    assert_eq!(publications.1.len(), 1);

    let second = repack(&mut store).unwrap();
    assert!(second.no_op);
    assert_eq!(second.generation_before, second.generation_after);
    drop(store);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert!(reopened.test_is_v2());
    for value in values {
        assert_value(&mut reopened, value);
    }
}

#[test]
fn ordinary_publication_after_repack_remains_v2() {
    let root = tempfile::tempdir().unwrap();
    let first = record(b"first generation");
    let second = record(b"later ordinary publication");
    let mut store =
        PackedCatalog::open_with_test_threshold(root.path(), crate::Durability::Durable, 1)
            .unwrap();
    store.put_records(&[first]).unwrap();
    repack(&mut store).unwrap();
    store.put_records(&[second]).unwrap();
    assert!(store.test_is_v2());
    drop(store);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert!(reopened.test_is_v2());
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
        store.test_set_repack_failpoint(failpoint);
        assert!(repack(&mut store).is_err());
        drop(store);

        let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
        for value in values {
            assert_value(&mut reopened, value);
        }
        assert!(!root.path().join("repack.intent").exists());
        assert_eq!(
            reopened.test_is_v2(),
            failpoint == RepackFailpoint::AfterHead
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
            legacy_content_generation(),
            authority,
        ),
        Err(CatalogError::RepackInventoryLimit { limit: 1 })
    ));
    assert_eq!(
        fs::read(root.path().join("catalog.head")).unwrap(),
        head_before
    );
}

fn write_catalog_record(
    directory: &std::path::Path,
    record: &wheat_core::SegmentCatalogRecord,
) -> [u8; 32] {
    let encoded = wheat_core::encode_segment_catalog_record(record).unwrap();
    let digest = *blake3::hash(&encoded).as_bytes();
    let digest_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(
        directory.join(format!("{}-{digest_hex}.catalog", record.generation)),
        encoded,
    )
    .unwrap();
    digest
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
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        2
    );
    store.close().unwrap();
    assert_eq!(
        fs::read_dir(root.path().join("segments")).unwrap().count(),
        3
    );
    assert_eq!(
        fs::read_dir(root.path().join("catalog")).unwrap().count(),
        3
    );
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
fn legacy_immutable_only_store_is_preserved_and_gains_an_active_pack() {
    let root = tempfile::tempdir().unwrap();
    let old = record(b"legacy immutable");
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
fn empty_active_without_head_recovers_but_nonempty_legacy_window_is_rejected() {
    let fresh = tempfile::tempdir().unwrap();
    {
        let _store = PackedCatalog::open(fresh.path(), crate::Durability::Durable).unwrap();
    }
    fs::remove_file(fresh.path().join("active.head")).unwrap();
    let _reopened = PackedCatalog::open(fresh.path(), crate::Durability::Durable).unwrap();

    let legacy = tempfile::tempdir().unwrap();
    let value = record(b"not published by a head");
    {
        let mut store = PackedCatalog::open(legacy.path(), crate::Durability::Durable).unwrap();
        store.put_records(&[value]).unwrap();
    }
    fs::remove_file(legacy.path().join("active.head")).unwrap();
    assert!(PackedCatalog::open(legacy.path(), crate::Durability::Durable).is_err());
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
fn legacy_head_discovery_ignores_malformed_and_orphan_higher_generations() {
    let root = tempfile::tempdir().unwrap();
    let value = record(b"valid chain root");
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
    let orphan = wheat_core::SegmentCatalogRecord {
        generation: 8,
        previous: None,
        segment: [9; 32],
        stored_length: 123,
    };
    write_catalog_record(&catalogs, &orphan);

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
    let authority = wheat_core::decode_segment_catalog_record(&catalog_bytes).unwrap();
    let head_digest = *blake3::hash(&catalog_bytes).as_bytes();
    let segment_hex = authority
        .segment
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for generation in [authority.generation + 1, authority.generation + 2] {
        let orphan_segment = format!("{generation}-{segment_hex}.segment");
        fs::write(segments.join(&orphan_segment), &segment_bytes).unwrap();
        write_catalog_record(
            &catalogs,
            &wheat_core::SegmentCatalogRecord {
                generation,
                previous: Some(head_digest),
                segment: authority.segment,
                stored_length: authority.stored_length,
            },
        );
    }
    // Fail closed for names or contents which cannot be proven to be a
    // canonical, authenticated immutable publication.
    fs::write(segments.join("01-deadbeef.segment"), b"preserve").unwrap();
    let malformed_segment = format!("99-{}.segment", "0".repeat(64));
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
    assert_eq!(authoritative.1.len(), 2);

    let mut reopened = PackedCatalog::open(root.path(), crate::Durability::Durable).unwrap();
    assert_value(&mut reopened, first);
    assert_value(&mut reopened, second);
    drop(reopened);
    assert_eq!(publication_names(root.path()), authoritative);
}

#[test]
fn legacy_head_discovery_rejects_duplicate_valid_latest_generation() {
    let root = tempfile::tempdir().unwrap();
    let catalogs = root.path().join("catalog");
    fs::create_dir_all(&catalogs).unwrap();
    for segment in [[1; 32], [2; 32]] {
        write_catalog_record(
            &catalogs,
            &wheat_core::SegmentCatalogRecord {
                generation: 0,
                previous: None,
                segment,
                stored_length: 1,
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
