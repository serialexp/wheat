use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{ObjectId, ViewId};
use wheat_store::{
    ImmutableObjectStore, ObjectStoreAdapter, ObjectStoreError, StoredObject, ViewLineageError,
    ViewLineageLimits, ViewLineageMapStore,
};

struct CountingStore<S> {
    inner: S,
    reads: AtomicUsize,
}

impl<S> CountingStore<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
        }
    }

    fn reset_reads(&self) {
        self.reads.store(0, Ordering::Relaxed);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for CountingStore<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

fn view(number: u32) -> ViewId {
    let mut digest = [0_u8; 32];
    digest[..4].copy_from_slice(&number.to_be_bytes());
    ViewId::from_digest(digest)
}

fn limits() -> ViewLineageLimits {
    ViewLineageLimits {
        max_structural_nodes: 10_000,
        max_map_nodes: 10_000,
        max_set_nodes: 10_000,
        max_decoded_bytes: 10 * 1024 * 1024,
        max_frontiers: 10_000,
        max_reachable_views: 10_000,
        max_edges: 10_000,
        max_pending: 10_000,
    }
}

#[tokio::test]
async fn grouped_edges_are_persistent_deterministic_and_dag_safe() {
    let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "view-lineage-correctness");
    let lineage = ViewLineageMapStore::new(&objects);
    let empty = lineage.empty_root().await.unwrap();
    let root = lineage
        .insert_edges(
            empty,
            [
                (view(1), view(3)),
                (view(1), view(2)),
                (view(1), view(2)),
                (view(2), view(4)),
                (view(3), view(4)),
                (view(3), view(5)),
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        lineage
            .descendant_frontiers(root, view(1), limits())
            .await
            .unwrap(),
        vec![view(4), view(5)]
    );
    assert!(
        lineage
            .is_descendant(root, view(1), view(4), limits())
            .await
            .unwrap()
    );
    assert!(
        !lineage
            .is_descendant(root, view(5), view(1), limits())
            .await
            .unwrap()
    );
    assert_eq!(
        lineage
            .descendant_frontiers(empty, view(1), limits())
            .await
            .unwrap(),
        vec![]
    );

    let cycle = lineage
        .insert_edges(root, [(view(4), view(1))])
        .await
        .unwrap();
    assert_eq!(
        lineage
            .descendant_frontiers(cycle, view(1), limits())
            .await
            .unwrap(),
        vec![view(5)]
    );
    assert_eq!(
        lineage
            .insert_edges(
                empty,
                [
                    (view(3), view(5)),
                    (view(3), view(4)),
                    (view(2), view(4)),
                    (view(1), view(2)),
                    (view(1), view(3)),
                ],
            )
            .await
            .unwrap(),
        root,
        "edge order and duplicates do not affect the authenticated root"
    );
}

#[tokio::test]
async fn traversal_does_not_scan_unrelated_retained_history() {
    const UNRELATED_EDGES: u32 = 10_000;
    let objects = CountingStore::new(ObjectStoreAdapter::new(
        Arc::new(InMemory::new()),
        "view-lineage-scale",
    ));
    let lineage = ViewLineageMapStore::new(&objects);
    let empty = lineage.empty_root().await.unwrap();
    let unrelated = (0..UNRELATED_EDGES).map(|index| {
        let parent = view(100_000 + index * 2);
        (parent, view(100_001 + index * 2))
    });
    let root = lineage
        .insert_edges(
            empty,
            unrelated.chain([(view(1), view(2)), (view(2), view(3))]),
        )
        .await
        .unwrap();

    objects.reset_reads();
    assert_eq!(
        lineage
            .descendant_frontiers(root, view(1), limits())
            .await
            .unwrap(),
        vec![view(3)]
    );
    let reads = objects.reads();
    assert!(
        reads < 40,
        "two-edge traversal read {reads} objects from a map with {UNRELATED_EDGES} unrelated edges"
    );
}

#[tokio::test]
async fn validates_complete_graph_and_monotonic_extensions() {
    let objects = CountingStore::new(ObjectStoreAdapter::new(
        Arc::new(InMemory::new()),
        "view-lineage-validation",
    ));
    let lineage = ViewLineageMapStore::new(&objects);
    let retained = wheat_store::RetainedSetStore::new(&objects);
    let retained_root = retained
        .update_batch(
            retained.empty_root().await.unwrap(),
            (1..=5).map(|id| (ObjectId::from(view(id)), true)),
        )
        .await
        .unwrap();
    let empty = lineage.empty_root().await.unwrap();
    let old = lineage
        .insert_edges(empty, [(view(1), view(2)), (view(1), view(3))])
        .await
        .unwrap();
    let new = lineage
        .insert_edges(old, [(view(2), view(4))])
        .await
        .unwrap();
    let validation = lineage
        .validate(new, retained_root, limits())
        .await
        .unwrap();
    assert_eq!(validation.edges, 3);
    assert_eq!(validation.vertices, 4);
    lineage
        .validate_extension(old, new, limits())
        .await
        .unwrap();

    objects.reset_reads();
    lineage
        .validate_extension(old, old, limits())
        .await
        .unwrap();
    assert_eq!(objects.reads(), 0);

    let dropped_child = lineage
        .insert_edges(empty, [(view(1), view(2))])
        .await
        .unwrap();
    assert!(matches!(
        lineage.validate_extension(old, dropped_child, limits()).await,
        Err(ViewLineageError::MissingEdge { parent, child })
            if parent == view(1) && child == view(3)
    ));
    assert!(matches!(
        lineage.validate_extension(old, empty, limits()).await,
        Err(ViewLineageError::MissingEdge { .. })
    ));

    let self_edge = lineage
        .insert_edges(empty, [(view(1), view(1))])
        .await
        .unwrap();
    assert!(matches!(
        lineage.validate(self_edge, retained_root, limits()).await,
        Err(ViewLineageError::SelfEdge(id)) if id == view(1)
    ));
    let cycle = lineage
        .insert_edges(empty, [(view(1), view(2)), (view(2), view(1))])
        .await
        .unwrap();
    assert!(matches!(
        lineage.validate(cycle, retained_root, limits()).await,
        Err(ViewLineageError::Cycle)
    ));
    let unretained = lineage
        .insert_edges(empty, [(view(1), view(9))])
        .await
        .unwrap();
    assert!(matches!(
        lineage.validate(unretained, retained_root, limits()).await,
        Err(ViewLineageError::ChildNotRetained(id)) if id == view(9)
    ));
}

#[tokio::test]
async fn traversal_bounds_are_independent() {
    let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "view-lineage-limits");
    let lineage = ViewLineageMapStore::new(&objects);
    let empty = lineage.empty_root().await.unwrap();
    let root = lineage
        .insert_edges(empty, [(view(1), view(2))])
        .await
        .unwrap();
    let generous = limits();

    assert!(matches!(
        lineage
            .descendant_frontiers(
                root,
                view(1),
                ViewLineageLimits {
                    max_structural_nodes: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::StructuralNodeLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .descendant_frontiers(
                root,
                view(1),
                ViewLineageLimits {
                    max_decoded_bytes: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::DecodedByteLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .descendant_frontiers(
                root,
                view(1),
                ViewLineageLimits {
                    max_frontiers: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::FrontierLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .descendant_frontiers(
                root,
                view(1),
                ViewLineageLimits {
                    max_reachable_views: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::ReachableViewLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .descendant_frontiers(
                root,
                view(1),
                ViewLineageLimits {
                    max_edges: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::EdgeLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .validate_extension(
                empty,
                root,
                ViewLineageLimits {
                    max_map_nodes: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::MapNodeLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .validate_extension(
                empty,
                root,
                ViewLineageLimits {
                    max_set_nodes: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::SetNodeLimitExceeded { limit: 0 })
    ));
    assert!(matches!(
        lineage
            .validate_extension(
                empty,
                root,
                ViewLineageLimits {
                    max_pending: 0,
                    ..generous
                }
            )
            .await,
        Err(ViewLineageError::PendingLimitExceeded { limit: 0 })
    ));
}
