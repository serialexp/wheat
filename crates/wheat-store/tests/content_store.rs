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
