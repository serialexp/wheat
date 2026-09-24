use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{ContentId, ContentSetId, ObjectId};
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
async fn digest_set_batches_write_only_changed_snapshots() {
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
    // A tiny set is cheaper as one snapshot than as a delta with a base reference.
    assert_eq!(objects.reset(), 1, "one snapshot with two members");
    // No-op edit produces the same root, no writes.
    assert_eq!(
        sets.update_batch(root, [(a, false), (a, true)])
            .await
            .unwrap(),
        root
    );
    assert_eq!(objects.reset(), 0);
    // Removing both members produces the empty root.
    assert_eq!(
        sets.update_batch(root, [(a, false), (b, false)])
            .await
            .unwrap(),
        empty
    );
    // With flat blobs: one put_if_absent call for the empty snapshot
    // (the backing store may dedup, but the call still happens).
    assert_eq!(objects.reset(), 1, "empty snapshot put_if_absent");
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
async fn resolve_rejects_missing_base_in_delta_chain() {
    let objects = InstrumentedStore::new();
    let sets = ContentSetStore::new(&objects);

    // Write a delta blob referencing a non-existent base.
    let fake_base = ContentSetId::from_digest([99; 32]);
    let a = ContentId::from_digest([1; 32]);
    let delta_bytes = wheat_core::content_set_delta_bytes(fake_base, &[a], &[]);
    let delta_id = ContentSetId::of_canonical_bytes(&delta_bytes);
    objects
        .put_if_absent(delta_id.into(), &delta_bytes)
        .await
        .unwrap();

    // Resolving the delta chain should fail because the base is missing.
    assert!(sets.resolve(delta_id).await.is_err());
    assert!(sets.contains(delta_id, a).await.is_err());
}
