use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use wheat_core::{ContentId, DirectoryNode, ObjectId};
use wheat_store::*;

fn object() -> (ObjectId, Vec<u8>) {
    let bytes = DirectoryNode::default().canonical_bytes();
    (ObjectId::of_canonical_bytes(&bytes), bytes)
}

fn blob(text: &str) -> (ContentId, Vec<u8>) {
    (ContentId::of(text.as_bytes()), text.as_bytes().to_vec())
}

fn durable_appends(trace: &PackedStoreTrace) -> u64 {
    trace.snapshot().unwrap().durable_appends
}

#[tokio::test]
async fn staged_objects_and_content_read_as_stored_and_flush_appends_once() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let base_text = "a line that the next version keeps\n".repeat(64);
    let base = content.put(base_text.as_bytes()).unwrap();
    let (object_id, object_bytes) = object();
    let (whole_id, whole) = blob("new content");
    let (changed_id, changed) = blob(&format!("{base_text}and one more line\n"));
    let before = durable_appends(&trace);

    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([
            (whole_id, whole.clone(), None),
            (changed_id, changed.clone(), Some(base)),
        ])
        .await
        .unwrap();

    assert_eq!(
        stage.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(stage.content().get(whole_id).unwrap(), whole);
    assert_eq!(
        stage.content().logical_length(changed_id).unwrap(),
        changed.len() as u64
    );
    assert_eq!(
        stage
            .content()
            .existing_ids(&[whole_id, changed_id])
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        store.get(object_id).await,
        Err(ObjectStoreError::Missing(_))
    ));
    assert!(!content.contains(whole_id));
    assert!(!content.contains(changed_id));
    assert_eq!(durable_appends(&trace), before);

    stage.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
    assert_eq!(
        store.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(content.get(whole_id).unwrap(), whole);
    assert_eq!(content.get(changed_id).unwrap(), changed);

    // Nothing is left staged to write again.
    stage.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
}

#[tokio::test]
async fn taken_stages_read_beneath_another_stage_and_store_in_one_append() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let (object_id, object_bytes) = object();
    let (blob_id, blob_bytes) = blob("accepted content");
    let before = durable_appends(&trace);

    let first = RepositoryStage::new(&store, 16, 1 << 20);
    first.put_if_absent(object_id, &object_bytes).await.unwrap();
    let second = RepositoryStage::new(&store, 16, 1 << 20);
    second
        .put_content([(blob_id, blob_bytes.clone(), None)])
        .await
        .unwrap();
    let mut pending = PendingPublications::default();
    pending.push(first.take().await.unwrap());
    pending.push(second.take().await.unwrap());

    // Taken stages hold nothing more to write.
    first.flush().await.unwrap();
    second.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before);

    // Another stage sees the pending publications only through the view.
    let third = RepositoryStage::new(&store, 16, 1 << 20);
    assert_eq!(third.exists_many(&[object_id]).await.unwrap(), [false]);
    assert!(third.content().get(blob_id).is_err());
    let view = pending.over(&third);
    assert_eq!(view.exists_many(&[object_id]).await.unwrap(), [true]);
    assert_eq!(
        view.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(view.content().get(blob_id).unwrap(), blob_bytes);

    pending.store(&store).await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
    assert_eq!(
        store.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(content.get(blob_id).unwrap(), blob_bytes);
}

#[tokio::test]
async fn content_reaching_the_byte_limit_is_stored_with_the_staged_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let (object_id, object_bytes) = object();
    let (small_id, small) = blob("small");
    let (large_id, large) = blob("larger than the stage holds");
    let before = durable_appends(&trace);

    let stage = RepositoryStage::new(&store, 16, 16);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage.put_content([(small_id, small, None)]).await.unwrap();
    assert!(!content.contains(small_id));
    stage.put_content([(large_id, large, None)]).await.unwrap();

    assert_eq!(durable_appends(&trace), before + 1);
    assert!(content.contains(small_id));
    assert!(content.contains(large_id));
    assert!(store.get(object_id).await.is_ok());
}

#[tokio::test]
async fn content_with_a_wrong_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = PackedObjectStore::open(dir.path()).unwrap();
    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    let (_, bytes) = blob("content");
    let error = stage
        .put_content([(ContentId::from_digest([7; 32]), bytes, None)])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RepositoryStoreError::Content(ContentStoreError::Corrupt { .. })
    ));
    assert!(!stage.content().contains(ContentId::from_digest([7; 32])));
}

struct FailingObjects {
    inner: LooseObjectStore,
    fail: AtomicBool,
}

#[async_trait]
impl ImmutableObjectStore for FailingObjects {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Backend("injected".into()));
        }
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[tokio::test]
async fn a_failed_flush_keeps_everything_staged_for_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = SeparateRepositoryStore::new(
        FailingObjects {
            inner: LooseObjectStore::new(dir.path().join("objects")),
            fail: AtomicBool::new(true),
        },
        ContentStore::new(dir.path().join("content")),
    );
    let (object_id, object_bytes) = object();
    let (content_id, bytes) = blob("content");
    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([(content_id, bytes.clone(), None)])
        .await
        .unwrap();

    assert!(stage.flush().await.is_err());
    assert!(stage.get(object_id).await.is_ok());
    assert_eq!(stage.content().get(content_id).unwrap(), bytes);

    store.objects().fail.store(false, Ordering::SeqCst);
    stage.flush().await.unwrap();
    assert!(store.get(object_id).await.is_ok());
    assert_eq!(store.content_store().get(content_id).unwrap(), bytes);
}
