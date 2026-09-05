use std::{fs, sync::Arc};

use wheat_core::{ContentId, FileEdit, ObjectId, Repository, Transformation, Tree};
use wheat_store::{ImmutableObjectStore, LooseObjectStore, ObjectKind, ObjectStoreError};

fn revision_and_view_bytes() -> (Vec<u8>, Vec<u8>) {
    let mut repository = Repository::new(Tree::new());
    let (_, outcome) = repository
        .create_change(
            [],
            Transformation {
                edits: vec![FileEdit {
                    path: "file".into(),
                    expected: None,
                    replacement: Some(ContentId::of(b"value")),
                }],
            },
        )
        .unwrap();
    (
        repository
            .revision(outcome.revision_id)
            .unwrap()
            .canonical_bytes(),
        repository.working_view().canonical_bytes(),
    )
}

#[tokio::test]
async fn deduplicates_and_reopens_canonical_objects() {
    let directory = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::new(directory.path());
    let (bytes, _) = revision_and_view_bytes();

    let first = store.put(&bytes).await.unwrap();
    let second = store.put(&bytes).await.unwrap();
    assert_eq!(first, second);

    let reopened = LooseObjectStore::new(directory.path());
    let stored = reopened
        .get_kind(first, ObjectKind::Revision)
        .await
        .unwrap();
    assert_eq!(stored.canonical_bytes, bytes);
    assert_eq!(reopened.exists_many(&[first]).await.unwrap(), vec![true]);
}

#[tokio::test]
async fn kind_tag_is_part_of_identity_and_checked_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::new(directory.path());
    let (revision_bytes, view_bytes) = revision_and_view_bytes();
    let revision = store.put(&revision_bytes).await.unwrap();
    let view = store.put(&view_bytes).await.unwrap();

    assert_ne!(revision, view);
    assert!(matches!(
        store.get_kind(revision, ObjectKind::View).await,
        Err(ObjectStoreError::WrongKind {
            expected: ObjectKind::View,
            actual: ObjectKind::Revision,
            ..
        })
    ));
}

#[tokio::test]
async fn distinguishes_missing_corrupt_and_invalid_objects() {
    let directory = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::new(directory.path());
    let (revision_bytes, _) = revision_and_view_bytes();
    let missing = ObjectId::of_canonical_bytes(&revision_bytes);
    assert!(matches!(
        store.get(missing).await,
        Err(ObjectStoreError::Missing(id)) if id == missing
    ));

    let stored = store.put(&revision_bytes).await.unwrap();
    fs::write(store.path(stored), [3, 1]).unwrap();
    assert!(matches!(
        store.get(stored).await,
        Err(ObjectStoreError::Corrupt { expected, .. }) if expected == stored
    ));

    assert!(matches!(
        store.put(&[3, 1]).await,
        Err(ObjectStoreError::InvalidFormat(_))
    ));
    let mut trailing = revision_bytes;
    trailing.push(0);
    assert!(matches!(
        store.put(&trailing).await,
        Err(ObjectStoreError::InvalidFormat(_))
    ));
}

#[tokio::test]
async fn rejects_a_claimed_id_that_does_not_match_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::new(directory.path());
    let (revision_bytes, view_bytes) = revision_and_view_bytes();
    let expected = ObjectId::of_canonical_bytes(&revision_bytes);

    assert!(matches!(
        store.put_if_absent(expected, &view_bytes).await,
        Err(ObjectStoreError::Corrupt { expected: id, .. }) if id == expected
    ));
    assert!(!store.path(expected).exists());
}

#[tokio::test]
async fn concurrent_identical_writes_leave_one_valid_object() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LooseObjectStore::new(directory.path()));
    let (revision_bytes, _) = revision_and_view_bytes();
    let bytes = Arc::new(revision_bytes);
    let threads = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let bytes = Arc::clone(&bytes);
            tokio::spawn(async move { store.put(&bytes).await.unwrap() })
        })
        .collect::<Vec<_>>();
    let mut ids = Vec::with_capacity(threads.len());
    for thread in threads {
        ids.push(thread.await.unwrap());
    }

    assert!(ids.iter().all(|id| *id == ids[0]));
    assert_eq!(store.get(ids[0]).await.unwrap().canonical_bytes, *bytes);
    let shard = store.path(ids[0]).parent().unwrap().to_path_buf();
    assert!(fs::read_dir(shard).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}
