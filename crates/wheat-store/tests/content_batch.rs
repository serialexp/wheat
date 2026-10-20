use wheat_core::VerifiedContent;
use wheat_store::{ContentStore, ContentStoreError, ContentWriteBatch};

#[test]
fn pending_tail_is_explicit_and_commit_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let content = ContentStore::new(directory.path());
    let mut batch = ContentWriteBatch::new(&content);
    let id = batch.put(b"pending").unwrap();
    assert!(matches!(
        content.get(id),
        Err(ContentStoreError::Missing(_))
    ));
    assert_eq!(batch.put(b"pending").unwrap(), id);
    batch.commit().unwrap();
    batch.commit().unwrap();
    assert_eq!(
        ContentStore::new(directory.path()).get(id).unwrap(),
        b"pending"
    );
}

#[test]
fn dropping_pending_tail_does_not_publish_content() {
    let directory = tempfile::tempdir().unwrap();
    let content = ContentStore::new(directory.path());
    let id = {
        let mut batch = ContentWriteBatch::new(&content);
        batch.put(b"abandoned").unwrap()
    };
    assert!(matches!(
        content.get(id),
        Err(ContentStoreError::Missing(_))
    ));
}

#[test]
fn oversized_content_flushes_older_tail_before_direct_write() {
    let directory = tempfile::tempdir().unwrap();
    let content = ContentStore::new(directory.path());
    let mut batch = ContentWriteBatch::new(&content);
    let first = batch.put(b"older tail").unwrap();
    let large = vec![7; 8 * 1024 * 1024 + 1];
    let second = batch.put(&large).unwrap();
    assert_eq!(content.get(first).unwrap(), b"older tail");
    assert_eq!(content.get(second).unwrap(), large);
}

#[test]
fn failed_commit_preserves_tail_for_retry() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("content");
    std::fs::write(&root, b"not a directory").unwrap();
    let content = ContentStore::new(&root);
    let mut batch = ContentWriteBatch::new(&content);
    let id = batch.put(b"retry").unwrap();
    assert!(batch.commit().is_err());
    std::fs::remove_file(&root).unwrap();
    batch.commit().unwrap();
    assert_eq!(content.get(id).unwrap(), b"retry");
}

#[test]
fn verified_content_is_staged_without_hashing_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let content = ContentStore::new(directory.path());
    let small = VerifiedContent::new(b"small".to_vec());
    let large = VerifiedContent::new(vec![7; 8 * 1024 * 1024 + 1]);

    let hashed = wheat_core::content_bytes_hashed();
    let mut batch = ContentWriteBatch::new(&content);
    batch.put_verified(small.clone()).unwrap();
    batch.put_verified(large.borrowed()).unwrap();
    batch.commit().unwrap();
    assert_eq!(wheat_core::content_bytes_hashed(), hashed);

    assert_eq!(content.get(small.id()).unwrap(), small.bytes());
    assert_eq!(content.get(large.id()).unwrap(), large.bytes());
}
