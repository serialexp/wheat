//! The frontier index answers what the forward walk does, and a query after a few new
//! edges reads what those edges touch, not the history under the label.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{ObjectId, ViewId, ViewLineageMapId};
use wheat_store::{
    FrontierIndex, ImmutableObjectStore, ObjectStoreAdapter, ObjectStoreError, StoredObject,
    ViewLineageError, ViewLineageLimits, ViewLineageMapStore,
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

    fn take_reads(&self) -> usize {
        self.reads.swap(0, Ordering::Relaxed)
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

fn store(name: &str) -> CountingStore<ObjectStoreAdapter> {
    CountingStore::new(ObjectStoreAdapter::new(Arc::new(InMemory::new()), name))
}

fn view(number: u32) -> ViewId {
    let mut digest = [0_u8; 32];
    digest[..4].copy_from_slice(&number.to_be_bytes());
    ViewId::from_digest(digest)
}

fn limits() -> ViewLineageLimits {
    ViewLineageLimits {
        max_structural_nodes: 100_000,
        max_map_nodes: 100_000,
        max_set_nodes: 100_000,
        max_decoded_bytes: 64 * 1024 * 1024,
        max_frontiers: 100_000,
        max_reachable_views: 100_000,
        max_edges: 100_000,
        max_pending: 100_000,
    }
}

/// SplitMix64, so the generated histories are the same on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

async fn expect_walk<S: ImmutableObjectStore>(
    index: &FrontierIndex,
    objects: &S,
    root: ViewLineageMapId,
    labels: &[ViewId],
    basis: ViewId,
    context: &str,
) {
    let walked = ViewLineageMapStore::new(objects)
        .descendant_frontiers(root, basis, limits())
        .await
        .unwrap();
    let indexed = index
        .frontiers(objects, root, labels.iter().copied(), basis, limits())
        .await
        .unwrap();
    assert_eq!(indexed, walked, "{context}: frontiers of {basis}");
}

/// Grows lineages at random: new views on top of others, joins of several parents,
/// edges between views that already exist (cycles included), several edges at once,
/// labels moving anywhere, and rewrites that drop edges. After every step each
/// label's frontiers from the index match the walk.
#[tokio::test]
async fn the_index_matches_the_walk_on_random_histories() {
    for seed in 0..12 {
        let objects = store("frontier-index-random");
        let lineage = ViewLineageMapStore::new(&objects);
        let directory = tempfile::tempdir().unwrap();
        let index = FrontierIndex::open(&directory.path().join("frontiers.sqlite")).unwrap();
        let mut random = Random(seed);
        let mut root = lineage.empty_root().await.unwrap();
        let mut edges = Vec::<(ViewId, ViewId)>::new();
        let mut next = 1;
        let mut labels = vec![view(0); 3];
        for step in 0..80 {
            match random.below(10) {
                0..=4 => {
                    let count = 1 + random.below(3);
                    let mut batch = Vec::new();
                    for _ in 0..count {
                        let parent = view(random.below(next as usize) as u32);
                        batch.push((parent, view(next)));
                        if random.below(4) == 0 {
                            let other = view(random.below(next as usize) as u32);
                            batch.push((other, view(next)));
                        }
                        next += 1;
                    }
                    edges.extend(&batch);
                    root = lineage.insert_edges(root, batch).await.unwrap();
                }
                5 => {
                    let edge = (
                        view(random.below(next as usize) as u32),
                        view(random.below(next as usize) as u32),
                    );
                    if edge.0 != edge.1 {
                        edges.push(edge);
                        root = lineage.insert_edges(root, [edge]).await.unwrap();
                    }
                }
                6 | 7 => {
                    let label = random.below(labels.len());
                    labels[label] = view(random.below(next as usize) as u32);
                }
                8 => {
                    edges.retain(|_| random.below(5) != 0);
                    let empty = lineage.empty_root().await.unwrap();
                    root = lineage
                        .insert_edges(empty, edges.iter().copied())
                        .await
                        .unwrap();
                }
                _ => {
                    let basis = view(random.below(next as usize) as u32);
                    expect_walk(
                        &index,
                        &objects,
                        root,
                        &labels,
                        basis,
                        &format!("seed {seed} step {step}, unlabelled basis"),
                    )
                    .await;
                }
            }
            for basis in labels.clone() {
                expect_walk(
                    &index,
                    &objects,
                    root,
                    &labels,
                    basis,
                    &format!("seed {seed} step {step}"),
                )
                .await;
            }
        }
    }
}

/// A label left in place while a chain of work grows on it: once the index holds the
/// label, a query after one more edge reads what that edge touched, whatever the
/// length of the chain. The walk reads the whole chain.
#[tokio::test]
async fn a_query_after_new_work_reads_what_the_work_touched() {
    const CHAIN: u32 = 400;
    let objects = store("frontier-index-chain");
    let lineage = ViewLineageMapStore::new(&objects);
    let directory = tempfile::tempdir().unwrap();
    let index = FrontierIndex::open(&directory.path().join("frontiers.sqlite")).unwrap();
    let label = view(0);
    let mut root = lineage.empty_root().await.unwrap();
    let mut reads = Vec::new();
    for tip in 1..=CHAIN {
        root = lineage
            .insert_edges(root, [(view(tip - 1), view(tip))])
            .await
            .unwrap();
        objects.take_reads();
        let frontiers = index
            .frontiers(&objects, root, [label], label, limits())
            .await
            .unwrap();
        assert_eq!(frontiers, vec![view(tip)]);
        reads.push(objects.take_reads());
    }
    // The first query builds the anchor with the walk; after it, one edge costs the
    // lineage diff and one child lookup.
    let most = reads[1..].iter().copied().max().unwrap();
    assert!(
        most <= 24,
        "a query after one new edge read up to {most} objects: {reads:?}"
    );

    lineage
        .descendant_frontiers(root, label, limits())
        .await
        .unwrap();
    let walked = objects.take_reads();
    assert!(
        walked > CHAIN as usize,
        "the walk read {walked} objects over a chain of {CHAIN}"
    );
}

/// The index outlives the process that built it: a second index on the same file
/// answers without walking, and one on a file it cannot read starts empty.
#[tokio::test]
async fn the_index_persists_and_rebuilds_from_a_damaged_file() {
    const CHAIN: u32 = 200;
    let objects = store("frontier-index-persist");
    let lineage = ViewLineageMapStore::new(&objects);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frontiers.sqlite");
    let label = view(0);
    let root = lineage
        .insert_edges(
            lineage.empty_root().await.unwrap(),
            (1..=CHAIN).map(|tip| (view(tip - 1), view(tip))),
        )
        .await
        .unwrap();
    {
        let index = FrontierIndex::open(&path).unwrap();
        index
            .frontiers(&objects, root, [label], label, limits())
            .await
            .unwrap();
    }
    objects.take_reads();
    let reopened = FrontierIndex::open(&path).unwrap();
    assert_eq!(
        reopened
            .frontiers(&objects, root, [label], label, limits())
            .await
            .unwrap(),
        vec![view(CHAIN)]
    );
    assert_eq!(
        objects.take_reads(),
        0,
        "a reopened index answers from the file"
    );
    drop(reopened);

    std::fs::write(&path, b"not a database").unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
    let damaged = FrontierIndex::open(&path).unwrap();
    assert_eq!(
        damaged
            .frontiers(&objects, root, [label], label, limits())
            .await
            .unwrap(),
        vec![view(CHAIN)]
    );
}

/// A label that moves leaves its old view out of the index: new work under the old
/// view is no longer written for it.
#[tokio::test]
async fn views_no_label_points_at_are_dropped() {
    let objects = store("frontier-index-evict");
    let lineage = ViewLineageMapStore::new(&objects);
    let directory = tempfile::tempdir().unwrap();
    let index = FrontierIndex::open(&directory.path().join("frontiers.sqlite")).unwrap();
    let mut root = lineage
        .insert_edges(lineage.empty_root().await.unwrap(), [(view(0), view(1))])
        .await
        .unwrap();
    index
        .frontiers(&objects, root, [view(0)], view(0), limits())
        .await
        .unwrap();
    // The label moves to view 1, and work piles up under it.
    for tip in 2..50 {
        root = lineage
            .insert_edges(root, [(view(tip - 1), view(tip))])
            .await
            .unwrap();
        assert_eq!(
            index
                .frontiers(&objects, root, [view(1)], view(1), limits())
                .await
                .unwrap(),
            vec![view(tip)]
        );
    }
    // Asked again, view 0 is built afresh with the walk, and still right.
    objects.take_reads();
    assert_eq!(
        index
            .frontiers(&objects, root, [view(1)], view(0), limits())
            .await
            .unwrap(),
        vec![view(49)]
    );
    assert!(
        objects.take_reads() > 49,
        "view 0 was rebuilt with the walk"
    );
}

/// The index holds an anchor to the walk's limits.
#[tokio::test]
async fn the_index_keeps_the_walk_limits() {
    let objects = store("frontier-index-limits");
    let lineage = ViewLineageMapStore::new(&objects);
    let directory = tempfile::tempdir().unwrap();
    let index = FrontierIndex::open(&directory.path().join("frontiers.sqlite")).unwrap();
    let root = lineage
        .insert_edges(
            lineage.empty_root().await.unwrap(),
            [(view(0), view(1)), (view(0), view(2))],
        )
        .await
        .unwrap();
    index
        .frontiers(&objects, root, [view(0)], view(0), limits())
        .await
        .unwrap();
    let error = index
        .frontiers(
            &objects,
            root,
            [view(0)],
            view(0),
            ViewLineageLimits {
                max_frontiers: 1,
                ..limits()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        wheat_store::FrontierIndexError::Lineage(ViewLineageError::FrontierLimitExceeded {
            limit: 1
        })
    ));
    let error = index
        .frontiers(
            &objects,
            root,
            [view(0)],
            view(0),
            ViewLineageLimits {
                max_reachable_views: 2,
                ..limits()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        wheat_store::FrontierIndexError::Lineage(ViewLineageError::ReachableViewLimitExceeded {
            limit: 2
        })
    ));
}
