use wheat_core::{ApplicationId, ChangeId, RevisionId, ViewId};
use wheat_git::journal::{ImportJournal, JournalError, NativeResult};

fn native(seed: u8) -> NativeResult {
    NativeResult {
        change_id: ChangeId::from_bytes([seed; 16]),
        revision_id: RevisionId::from_digest([seed; 32]),
        view_id: ViewId::from_digest([seed.wrapping_add(1); 32]),
        application_id: ApplicationId::from_digest([seed.wrapping_add(2); 32]),
    }
}

#[test]
fn restart_preserves_pending_and_completed_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("import.sqlite");
    {
        let mut journal = ImportJournal::open(&path, b"snapshot").unwrap();
        journal.add_pending(b"root", &[]).unwrap();
        journal.add_pending(b"child", &[b"root".to_vec()]).unwrap();
        journal.complete(b"root", native(1)).unwrap();
    }
    let journal = ImportJournal::open(&path, b"snapshot").unwrap();
    assert_eq!(journal.result(b"root").unwrap(), Some(native(1)));
    assert_eq!(journal.result(b"child").unwrap(), None);
    assert_eq!(journal.ready(1).unwrap()[0].oid, b"child");
}

#[test]
fn journals_from_before_or_after_the_prepared_protocol_are_rejected() {
    for version in [1, 3] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("import.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE journal_identity (singleton INTEGER PRIMARY KEY, format_version INTEGER NOT NULL, source_snapshot BLOB NOT NULL);\
                 INSERT INTO journal_identity VALUES (1, {version}, X'736E617073686F74');"
            ))
            .unwrap();
        drop(connection);

        assert!(matches!(
            ImportJournal::open(&path, b"snapshot"),
            Err(JournalError::SnapshotMismatch)
        ));
    }
}

#[test]
fn source_snapshot_mismatch_fails() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("import.sqlite");
    drop(ImportJournal::open(&path, b"first").unwrap());
    assert!(matches!(
        ImportJournal::open(&path, b"second"),
        Err(JournalError::SnapshotMismatch)
    ));
}

#[test]
fn ready_requires_every_parent_and_preserves_parent_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("import.sqlite");
    let mut journal = ImportJournal::open(path, b"snapshot").unwrap();
    journal.add_pending(b"first", &[]).unwrap();
    journal.add_pending(b"second", &[]).unwrap();
    journal
        .add_pending(b"merge", &[b"first".to_vec(), b"second".to_vec()])
        .unwrap();
    journal.complete(b"first", native(1)).unwrap();
    assert_eq!(journal.ready(10).unwrap()[0].oid, b"second");
    journal.complete(b"second", native(2)).unwrap();
    let ready = journal.ready(10).unwrap();
    assert_eq!(ready[0].oid, b"merge");
    assert_eq!(ready[0].parents, [b"first".to_vec(), b"second".to_vec()]);
}

#[test]
fn duplicate_completion_is_idempotent_but_conflict_fails() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal.add_pending(b"oid", &[]).unwrap();
    journal.complete(b"oid", native(1)).unwrap();
    journal.complete(b"oid", native(1)).unwrap();
    assert!(matches!(
        journal.complete(b"oid", native(2)),
        Err(JournalError::ConflictingCompletion)
    ));
}

#[test]
fn completion_cannot_skip_missing_or_incomplete_parent() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal
        .add_pending(b"child", &[b"parent".to_vec()])
        .unwrap();
    assert!(matches!(
        journal.complete(b"child", native(2)),
        Err(JournalError::IncompleteParents)
    ));
    journal.add_pending(b"parent", &[]).unwrap();
    assert!(matches!(
        journal.complete(b"child", native(2)),
        Err(JournalError::IncompleteParents)
    ));
    assert_eq!(journal.result(b"child").unwrap(), None);
    journal.complete(b"parent", native(1)).unwrap();
    journal.complete(b"child", native(2)).unwrap();
}

#[test]
fn prepared_parents_schedule_dependents_without_becoming_completed() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal.add_pending(b"root", &[]).unwrap();
    journal.add_pending(b"child", &[b"root".to_vec()]).unwrap();

    journal.begin_preparing().unwrap();
    journal.prepare(b"root", native(1)).unwrap();
    assert_eq!(journal.result(b"root").unwrap(), None);
    assert_eq!(journal.mapping(b"root").unwrap(), Some(native(1)));
    assert_eq!(journal.ready(1).unwrap()[0].oid, b"child");
    journal.prepare(b"child", native(2)).unwrap();
    journal.commit_preparing().unwrap();

    let prepared = journal.prepared(2).unwrap();
    assert_eq!(
        prepared
            .iter()
            .map(|(oid, _)| oid.as_slice())
            .collect::<Vec<_>>(),
        vec![b"root".as_slice(), b"child".as_slice()]
    );
    journal.complete_many(&prepared).unwrap();
    assert_eq!(journal.result(b"root").unwrap(), Some(native(1)));
    assert_eq!(journal.result(b"child").unwrap(), Some(native(2)));
}

#[test]
fn complete_many_accepts_a_child_before_its_parent_in_the_same_set() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal.add_pending(b"root", &[]).unwrap();
    journal.add_pending(b"child", &[b"root".to_vec()]).unwrap();
    journal.begin_preparing().unwrap();
    journal.prepare(b"root", native(1)).unwrap();
    journal.prepare(b"child", native(2)).unwrap();
    journal.commit_preparing().unwrap();

    journal
        .complete_many(&[
            (b"child".to_vec(), native(2)),
            (b"root".to_vec(), native(1)),
        ])
        .unwrap();
    assert_eq!(journal.result(b"root").unwrap(), Some(native(1)));
    assert_eq!(journal.result(b"child").unwrap(), Some(native(2)));
}

#[test]
fn complete_many_rejects_a_subset_without_its_prepared_parent_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal.add_pending(b"root", &[]).unwrap();
    journal.add_pending(b"child", &[b"root".to_vec()]).unwrap();
    journal.begin_preparing().unwrap();
    journal.prepare(b"root", native(1)).unwrap();
    journal.prepare(b"child", native(2)).unwrap();
    journal.commit_preparing().unwrap();

    assert!(matches!(
        journal.complete_many(&[(b"child".to_vec(), native(2))]),
        Err(JournalError::IncompleteParents)
    ));
    assert_eq!(journal.result(b"root").unwrap(), None);
    assert_eq!(journal.result(b"child").unwrap(), None);
    assert_eq!(journal.prepared(2).unwrap().len(), 2);
}

#[test]
fn complete_many_rejects_a_missing_parent_even_if_the_child_is_prepared() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("import.sqlite");
    let mut journal = ImportJournal::open(&path, b"snapshot").unwrap();
    journal
        .add_pending(b"child", &[b"missing".to_vec()])
        .unwrap();
    drop(journal);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO prepared_work(oid, change_id, revision_id, view_id, application_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![b"child", native(2).change_id.as_bytes().as_slice(), native(2).revision_id.as_bytes().as_slice(), native(2).view_id.as_bytes().as_slice(), native(2).application_id.as_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);
    let mut journal = ImportJournal::open(&path, b"snapshot").unwrap();

    assert!(matches!(
        journal.complete_many(&[(b"child".to_vec(), native(2))]),
        Err(JournalError::IncompleteParents)
    ));
    assert_eq!(journal.result(b"child").unwrap(), None);
}

#[test]
fn complete_many_is_atomic_when_any_prepared_mapping_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    journal.add_pending(b"first", &[]).unwrap();
    journal.add_pending(b"second", &[]).unwrap();
    journal.begin_preparing().unwrap();
    journal.prepare(b"first", native(1)).unwrap();
    journal.prepare(b"second", native(2)).unwrap();
    journal.commit_preparing().unwrap();

    assert!(matches!(
        journal.complete_many(&[
            (b"first".to_vec(), native(1)),
            (b"second".to_vec(), native(3)),
        ]),
        Err(JournalError::ConflictingCompletion)
    ));
    assert_eq!(journal.result(b"first").unwrap(), None);
    assert_eq!(journal.result(b"second").unwrap(), None);
    assert_eq!(journal.prepared(2).unwrap().len(), 2);
}

#[test]
fn exclusive_ownership_is_released_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("import.sqlite");
    let first = ImportJournal::open(&path, b"snapshot").unwrap();
    assert!(matches!(
        ImportJournal::open(&path, b"snapshot"),
        Err(JournalError::AlreadyOwned(_))
    ));
    drop(first);
    assert!(ImportJournal::open(&path, b"snapshot").is_ok());
}

#[test]
fn completed_mapping_pages_are_bounded_and_exhaustive() {
    let directory = tempfile::tempdir().unwrap();
    let mut journal =
        ImportJournal::open(directory.path().join("import.sqlite"), b"snapshot").unwrap();
    for n in (0..7u8).rev() {
        journal.add_pending(&[n], &[]).unwrap();
        journal.complete(&[n], native(n)).unwrap();
    }
    let mut after = None::<Vec<u8>>;
    let mut seen = Vec::new();
    loop {
        let page = journal.completed(after.as_deref(), 2).unwrap();
        assert!(page.len() < 3);
        if page.is_empty() {
            break;
        }
        for (oid, _) in page {
            seen.push(oid[0]);
            after = Some(oid);
        }
    }
    assert_eq!(seen, (0..7).collect::<Vec<_>>());
}
