use wheat_core::{DirectoryNode, ObjectId};
use wheat_store::*;
#[tokio::test]
async fn pending_objects_are_readable_but_only_flush_makes_them_durable() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let bytes = DirectoryNode::default().canonical_bytes();
    let id = ObjectId::of_canonical_bytes(&bytes);
    let batch = BatchObjectStore::new(&objects, 16, 4096);
    batch.put_if_absent(id, &bytes).await.unwrap();
    assert_eq!(batch.get(id).await.unwrap().canonical_bytes, bytes);
    assert!(matches!(
        objects.get(id).await,
        Err(ObjectStoreError::Missing(_))
    ));
    batch.flush().await.unwrap();
    assert_eq!(objects.get(id).await.unwrap().canonical_bytes, bytes);
    batch.flush().await.unwrap();
}
#[tokio::test]
async fn invalid_ids_never_enter_batch() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let batch = BatchObjectStore::new(&objects, 16, 4096);
    let bytes = DirectoryNode::default().canonical_bytes();
    assert!(
        batch
            .put_if_absent(ObjectId::from_digest([3; 32]), &bytes)
            .await
            .is_err()
    );
    batch.flush().await.unwrap();
}

use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
struct Failing {
    inner: LooseObjectStore,
    fail: AtomicBool,
}
#[async_trait]
impl ImmutableObjectStore for Failing {
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
    async fn put_if_absent(&self, id: ObjectId, b: &[u8]) -> Result<(), ObjectStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Backend("injected".into()));
        }
        self.inner.put_if_absent(id, b).await
    }
}
#[tokio::test]
async fn failed_flush_retains_pending_bytes_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let backing = Failing {
        inner: LooseObjectStore::new(dir.path()),
        fail: AtomicBool::new(true),
    };
    let batch = BatchObjectStore::new(&backing, 16, 4096);
    let bytes = DirectoryNode::default().canonical_bytes();
    let id = ObjectId::of_canonical_bytes(&bytes);
    batch.put_if_absent(id, &bytes).await.unwrap();
    assert!(batch.flush().await.is_err());
    assert_eq!(batch.get(id).await.unwrap().canonical_bytes, bytes);
    backing.fail.store(false, Ordering::SeqCst);
    batch.flush().await.unwrap();
    assert_eq!(backing.get(id).await.unwrap().canonical_bytes, bytes);
}
