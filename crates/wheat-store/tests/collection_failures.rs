use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{ContentId, ContentSetChild, ContentSetId, ContentSetNode, ObjectId};
use wheat_store::{
    ContentSetStore, ImmutableObjectStore, ObjectStoreAdapter, ObjectStoreError, StoredObject,
};

struct InstrumentedStore {
    inner: ObjectStoreAdapter,
    writes: AtomicUsize,
    fail_at: AtomicUsize,
}

impl InstrumentedStore {
    fn new() -> Self {
        Self {
            inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "instrumented-collections"),
            writes: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(usize::MAX),
        }
    }

    fn reset(&self) -> usize {
        self.writes.swap(0, Ordering::SeqCst)
    }
}

#[async_trait]
impl ImmutableObjectStore for InstrumentedStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        let write = self.writes.fetch_add(1, Ordering::SeqCst);
        if write == self.fail_at.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Backend("injected write failure".into()));
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
async fn digest_set_batches_write_only_changed_prefixes() {
    let objects = InstrumentedStore::new();
    let sets = ContentSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    objects.reset();
    let a = ContentId::from_digest([0; 32]);
    let mut bytes = [0; 32];
    bytes[31] = 1;
    let b = ContentId::from_digest(bytes);
    let root = sets
        .update_batch(empty, [(a, true), (b, true)])
        .await
        .unwrap();
    assert_eq!(objects.reset(), 1, "one root with two inline entries");
    assert_eq!(
        sets.update_batch(root, [(a, false), (a, true)])
            .await
            .unwrap(),
        root
    );
    assert_eq!(objects.reset(), 0);
    assert_eq!(
        sets.update_batch(root, [(a, false), (b, false)])
            .await
            .unwrap(),
        empty
    );
    assert_eq!(
        objects.reset(),
        2,
        "incremental removal: collapse to single leaf, then to empty root"
    );
}

#[tokio::test]
async fn failed_batch_keeps_old_root_readable_and_retry_is_deterministic() {
    let objects = InstrumentedStore::new();
    let sets = ContentSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = ContentId::from_digest([1; 32]);
    let b = ContentId::from_digest([2; 32]);
    let old = sets.insert(empty, a).await.unwrap();
    objects.reset();
    objects.fail_at.store(0, Ordering::SeqCst);
    assert!(sets.insert(old, b).await.is_err());
    assert!(sets.contains(old, a).await.unwrap());
    assert!(!sets.contains(old, b).await.unwrap());
    objects.fail_at.store(usize::MAX, Ordering::SeqCst);
    let retried = sets.insert(old, b).await.unwrap();
    let fresh = sets
        .update_batch(empty, [(b, true), (a, true)])
        .await
        .unwrap();
    assert_eq!(retried, fresh);
}

#[tokio::test]
async fn cursor_rejects_wrong_depth_and_missing_children_without_skipping_them() {
    let objects = InstrumentedStore::new();
    let sets = ContentSetStore::new(&objects);
    for missing in [false, true] {
        let child = ContentSetNode::new(vec![2], true, vec![], vec![]).unwrap();
        let child_id = if missing {
            ContentSetId::from_digest([99; 32])
        } else {
            objects
                .put_if_absent(child.id().into(), &child.canonical_bytes())
                .await
                .unwrap();
            child.id()
        };
        let root = ContentSetNode::new(
            vec![],
            false,
            vec![
                ContentSetChild {
                    edge: 0,
                    child: child_id,
                },
                ContentSetChild {
                    edge: 1,
                    child: child_id,
                },
            ],
            vec![],
        )
        .unwrap();
        objects
            .put_if_absent(root.id().into(), &root.canonical_bytes())
            .await
            .unwrap();
        let mut cursor = sets.cursor(root.id()).await.unwrap();
        assert!(cursor.next().await.is_err());
        assert_eq!(
            cursor.next().await.unwrap(),
            None,
            "failure exhausts cursor"
        );
        assert!(
            sets.contains(root.id(), ContentId::from_digest([0; 32]))
                .await
                .is_err()
        );
    }
}
