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
