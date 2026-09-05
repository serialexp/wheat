use std::fs;

use wheat_core::ContentId;
use wheat_store::{ContentStore, ContentStoreError};

#[test]
fn retains_reads_and_deduplicates_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    let first = store.put(b"same bytes").unwrap();
    let second = store.put(b"same bytes").unwrap();

    assert_eq!(first, second);
    assert_eq!(store.get(first).unwrap(), b"same bytes");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn distinguishes_missing_and_corrupt_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let missing = ContentId::of(b"missing");
    assert!(matches!(store.get(missing), Err(ContentStoreError::Missing(id)) if id == missing));

    let stored = store.put(b"original").unwrap();
    fs::write(store.path(stored), b"corrupt").unwrap();
    assert!(matches!(
        store.get(stored),
        Err(ContentStoreError::Corrupt { expected, .. }) if expected == stored
    ));
}

#[test]
fn batch_retention_is_parallel_deduplicated_and_integrity_checked() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let first = ContentId::of(b"first");
    let second = ContentId::of(b"second");

    store
        .put_many(
            [
                (first, b"first".to_vec()),
                (second, b"second".to_vec()),
                (first, b"first".to_vec()),
            ],
            4,
        )
        .unwrap();
    assert_eq!(store.get(first).unwrap(), b"first");
    assert_eq!(store.get(second).unwrap(), b"second");

    let error = store
        .put_many([(first, b"not first".to_vec())], 2)
        .unwrap_err();
    assert!(matches!(
        error,
        ContentStoreError::Corrupt { expected, .. } if expected == first
    ));
}
