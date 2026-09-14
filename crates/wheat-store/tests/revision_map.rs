use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{
    ChangeId, ObjectId, RevisionId, RevisionMapChild, RevisionMapId, RevisionMapNode,
};
use wheat_store::{
    ImmutableObjectStore, LooseObjectStore, ObjectStoreAdapter, ObjectStoreError, RevisionMapDelta,
    RevisionMapDeltaLimits, RevisionMapError, RevisionMapStore, StoredObject,
};

fn change(value: &str) -> ChangeId {
    ChangeId::from_str(value).unwrap()
}

fn memory_store(prefix: &str) -> ObjectStoreAdapter {
    ObjectStoreAdapter::new(Arc::new(InMemory::new()), prefix)
}

async fn conformance<S: ImmutableObjectStore>(objects: &S) {
    let maps = RevisionMapStore::new(objects);
    let empty = maps.empty_root().await.unwrap();
    let first = change("01890f2d-3c4b-7a68-9abc-def012345678");
    let sibling = change("01890f2d-3c4b-7a68-9abc-def012345679");
    let distant = change("ff890f2d-3c4b-7a68-9abc-def012345678");
    let revision_a = RevisionId::from_digest([1; 32]);
    let revision_b = RevisionId::from_digest([2; 32]);
    let revision_c = RevisionId::from_digest([3; 32]);

    let one = maps.insert(empty, first, revision_a).await.unwrap();
    let two = maps.insert(one, sibling, revision_b).await.unwrap();
    let three = maps.insert(two, distant, revision_c).await.unwrap();
    assert_eq!(maps.get(three, first).await.unwrap(), Some(revision_a));
    assert_eq!(maps.get(three, sibling).await.unwrap(), Some(revision_b));
    assert_eq!(maps.get(three, distant).await.unwrap(), Some(revision_c));
    assert_eq!(maps.insert(three, first, revision_a).await.unwrap(), three);

    let replaced = maps.insert(three, first, revision_c).await.unwrap();
    assert_eq!(maps.get(replaced, first).await.unwrap(), Some(revision_c));
    assert_eq!(maps.get(three, first).await.unwrap(), Some(revision_a));
    let removed = maps.remove(replaced, sibling).await.unwrap();
    assert_eq!(maps.get(removed, sibling).await.unwrap(), None);
    assert_eq!(maps.get(removed, distant).await.unwrap(), Some(revision_c));
    assert!(maps.contains_node(removed).await.unwrap());
}

struct CountingStore<S> {
    inner: S,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl<S> CountingStore<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        }
    }

    fn take_reads(&self) -> usize {
        self.reads.swap(0, Ordering::SeqCst)
    }

    fn take_writes(&self) -> usize {
        self.writes.swap(0, Ordering::SeqCst)
    }
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for CountingStore<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[tokio::test]
async fn loose_store_persists_copy_on_write_revision_maps() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    conformance(&objects).await;

    let maps = RevisionMapStore::new(&objects);
    let key = change("01890f2d-3c4b-7a68-9abc-def012345678");
    let value = RevisionId::from_digest([8; 32]);
    let empty = maps.empty_root().await.unwrap();
    let root = maps.insert(empty, key, value).await.unwrap();
    let reopened = LooseObjectStore::new(directory.path());
    assert_eq!(
        RevisionMapStore::new(&reopened)
            .get(root, key)
            .await
            .unwrap(),
        Some(value)
    );
}

#[tokio::test]
async fn apache_memory_store_passes_revision_map_conformance() {
    let objects = memory_store("revision-map");
    conformance(&objects).await;
}

#[tokio::test]
async fn batch_groups_shared_paths_and_prunes_deletions() {
    let objects = CountingStore::new(memory_store("batch-write-counts"));
    let maps = RevisionMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    assert_eq!(objects.take_writes(), 1);

    let mut first_bytes = [0x42; 16];
    first_bytes[15] = 1;
    let mut second_bytes = first_bytes;
    second_bytes[15] = 2;
    let first = ChangeId::from_bytes(first_bytes);
    let second = ChangeId::from_bytes(second_bytes);
    let absent = ChangeId::from_bytes([0x99; 16]);
    let revision_a = RevisionId::from_digest([0xa1; 32]);
    let revision_b = RevisionId::from_digest([0xb2; 32]);

    let one = maps
        .update_batch(empty, [(first, Some(revision_a))])
        .await
        .unwrap();
    assert_eq!(
        objects.take_writes(),
        1,
        "one key writes one compressed leaf"
    );

    let no_op = maps
        .update_batch(
            one,
            [
                (first, Some(revision_b)),
                (first, Some(revision_a)),
                (absent, None),
            ],
        )
        .await
        .unwrap();
    assert_eq!(no_op, one);
    assert_eq!(
        objects.take_writes(),
        0,
        "last duplicate wins and a semantically empty batch writes nothing"
    );

    let two = maps
        .update_batch(
            empty,
            [(first, Some(revision_a)), (second, Some(revision_b))],
        )
        .await
        .unwrap();
    assert_eq!(
        objects.take_writes(),
        3,
        "two leaves and their compressed branch are written once"
    );
    assert_eq!(maps.get(two, first).await.unwrap(), Some(revision_a));
    assert_eq!(maps.get(two, second).await.unwrap(), Some(revision_b));

    let removed = maps.update_batch(one, [(first, None)]).await.unwrap();
    assert_eq!(removed, empty);
    assert_eq!(
        objects.take_writes(),
        1,
        "deleting the sole key writes only the root, not empty child nodes"
    );
}

#[tokio::test]
async fn update_batch_uses_the_last_duplicate_value() {
    let objects = memory_store("batch-duplicates");
    let maps = RevisionMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    let key = ChangeId::from_bytes([7; 16]);
    let first = RevisionId::from_digest([1; 32]);
    let last = RevisionId::from_digest([2; 32]);

    let root = maps
        .update_batch(empty, [(key, Some(first)), (key, None), (key, Some(last))])
        .await
        .unwrap();
    assert_eq!(maps.get(root, key).await.unwrap(), Some(last));

    let root = maps
        .update_batch(root, [(key, Some(first)), (key, None)])
        .await
        .unwrap();
    assert_eq!(root, empty);
}

async fn assert_ordered_cursor<S: ImmutableObjectStore>(objects: &S, root: RevisionMapId) {
    let maps = RevisionMapStore::new(objects);
    let mut cursor = maps.cursor(root).await.unwrap();
    let mut previous = None;
    let mut count = 0;
    while let Some((key, _)) = cursor.next().await.unwrap() {
        if let Some(previous) = previous {
            assert!(previous < key, "cursor keys must be strictly ordered");
        }
        previous = Some(key);
        count += 1;
    }
    assert_eq!(count, 4);
    assert_eq!(cursor.next().await.unwrap(), None);
}

#[tokio::test]
async fn cursor_is_ordered_and_loose_cursor_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let root = {
        let objects = LooseObjectStore::new(directory.path());
        let maps = RevisionMapStore::new(&objects);
        let empty = maps.empty_root().await.unwrap();
        maps.update_batch(
            empty,
            [9_u8, 1, 200, 17].map(|byte| {
                (
                    ChangeId::from_bytes([byte; 16]),
                    Some(RevisionId::from_digest([byte; 32])),
                )
            }),
        )
        .await
        .unwrap()
    };

    let reopened = LooseObjectStore::new(directory.path());
    assert_ordered_cursor(&reopened, root).await;
}

#[tokio::test]
async fn deterministic_batches_match_btree_map_and_cursor_order() {
    let objects = memory_store("batch-differential");
    let maps = RevisionMapStore::new(&objects);
    let mut root = maps.empty_root().await.unwrap();
    let mut expected = BTreeMap::new();
    let mut state = 0x6a09_e667_f3bc_c909_u64;

    for batch_index in 0..32_u8 {
        let mut batch = Vec::new();
        for operation in 0..24_u8 {
            // A fixed LCG keeps this test reproducible without adding a rand dependency.
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let slot = (state >> 57) as u8 % 19;
            let mut key_bytes = [0x5a; 16];
            key_bytes[15] = slot; // Deliberate repeated keys within and across batches.
            let key = ChangeId::from_bytes(key_bytes);
            let value = if state & 3 == 0 {
                None
            } else {
                Some(RevisionId::from_digest(
                    [batch_index.wrapping_mul(24).wrapping_add(operation); 32],
                ))
            };
            batch.push((key, value));
            match value {
                Some(value) => {
                    expected.insert(key, value);
                }
                None => {
                    expected.remove(&key);
                }
            }
        }

        root = maps.update_batch(root, batch).await.unwrap();
        let mut cursor = maps.cursor(root).await.unwrap();
        let mut actual = Vec::new();
        while let Some(entry) = cursor.next().await.unwrap() {
            actual.push(entry);
        }
        assert_eq!(
            actual,
            expected.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn cursor_reports_a_missing_child_during_iteration() {
    let objects = memory_store("cursor-missing-child");
    let missing = RevisionMapId::from_digest([0xee; 32]);
    let root = RevisionMapNode::new(
        vec![],
        None,
        vec![
            RevisionMapChild {
                edge: 42,
                child: missing,
            },
            RevisionMapChild {
                edge: 43,
                child: missing,
            },
        ],
    )
    .unwrap();
    objects
        .put_if_absent(root.id().into(), &root.canonical_bytes())
        .await
        .unwrap();

    let maps = RevisionMapStore::new(&objects);
    let mut cursor = maps.cursor(root.id()).await.unwrap();
    assert!(matches!(
        cursor.next().await,
        Err(RevisionMapError::Store(ObjectStoreError::Missing(id))) if id == missing.into()
    ));
}

#[tokio::test]
async fn delta_reports_ordered_added_changed_and_removed_selections() {
    let objects = CountingStore::new(memory_store("revision-map-delta"));
    let maps = RevisionMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    let keys = [1_u8, 2, 3].map(|byte| ChangeId::from_bytes([byte; 16]));
    let revisions = [1_u8, 2, 3, 4].map(|byte| RevisionId::from_digest([byte; 32]));
    let basis = maps
        .update_batch(
            empty,
            [(keys[0], Some(revisions[0])), (keys[1], Some(revisions[1]))],
        )
        .await
        .unwrap();
    let frontier = maps
        .update_batch(
            basis,
            [
                (keys[0], None),
                (keys[1], Some(revisions[2])),
                (keys[2], Some(revisions[3])),
            ],
        )
        .await
        .unwrap();
    objects.take_reads();

    assert_eq!(
        maps.delta(basis, frontier, RevisionMapDeltaLimits::default())
            .await
            .unwrap(),
        vec![
            RevisionMapDelta::Removed {
                change: keys[0],
                revision: revisions[0],
            },
            RevisionMapDelta::Changed {
                change: keys[1],
                basis: revisions[1],
                frontier: revisions[2],
            },
            RevisionMapDelta::Added {
                change: keys[2],
                revision: revisions[3],
            },
        ]
    );

    objects.take_reads();
    assert!(
        maps.delta(basis, basis, RevisionMapDeltaLimits::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        objects.take_reads(),
        0,
        "equal roots require no store reads"
    );
}

#[tokio::test]
async fn delta_prunes_a_representative_ten_thousand_entry_map() {
    let objects = CountingStore::new(memory_store("revision-map-delta-large"));
    let maps = RevisionMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    let entries = (0_u32..10_000).map(|number| {
        let mut bytes = [0_u8; 16];
        bytes[12..].copy_from_slice(&number.to_be_bytes());
        (
            ChangeId::from_bytes(bytes),
            Some(RevisionId::from_digest([number as u8; 32])),
        )
    });
    let basis = maps.update_batch(empty, entries).await.unwrap();
    let changed_key = {
        let mut bytes = [0_u8; 16];
        bytes[12..].copy_from_slice(&5_000_u32.to_be_bytes());
        ChangeId::from_bytes(bytes)
    };
    let replacement = RevisionId::from_digest([0xfe; 32]);
    let frontier = maps.insert(basis, changed_key, replacement).await.unwrap();
    objects.take_reads();

    assert_eq!(
        maps.delta(basis, frontier, RevisionMapDeltaLimits::default())
            .await
            .unwrap(),
        vec![RevisionMapDelta::Changed {
            change: changed_key,
            basis: RevisionId::from_digest([5_000_u32 as u8; 32]),
            frontier: replacement,
        }]
    );
    let reads = objects.take_reads();
    assert!(
        reads <= 34,
        "one-key delta read {reads} nodes, expected O(key depth)"
    );
}

#[tokio::test]
async fn delta_enforces_selection_node_and_pending_bounds() {
    let objects = memory_store("revision-map-delta-bounds");
    let maps = RevisionMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    let first = ChangeId::from_bytes([1; 16]);
    let second = ChangeId::from_bytes([2; 16]);
    let root = maps
        .update_batch(
            empty,
            [
                (first, Some(RevisionId::from_digest([1; 32]))),
                (second, Some(RevisionId::from_digest([2; 32]))),
            ],
        )
        .await
        .unwrap();

    assert!(matches!(
        maps.delta(
            empty,
            root,
            RevisionMapDeltaLimits {
                max_selections: 1,
                ..Default::default()
            },
        )
        .await,
        Err(RevisionMapError::DeltaSelectionLimitExceeded { limit: 1 })
    ));
    assert!(matches!(
        maps.delta(
            empty,
            root,
            RevisionMapDeltaLimits {
                max_nodes: 1,
                ..Default::default()
            },
        )
        .await,
        Err(RevisionMapError::DeltaNodeLimitExceeded { limit: 1 })
    ));
    assert!(matches!(
        maps.delta(
            empty,
            root,
            RevisionMapDeltaLimits {
                max_pending: 0,
                ..Default::default()
            },
        )
        .await,
        Err(RevisionMapError::DeltaPendingLimitExceeded { limit: 0 })
    ));
}

#[tokio::test]
async fn missing_revision_map_node_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let maps = RevisionMapStore::new(&objects);
    let missing = RevisionMapId::from_digest([9; 32]);
    assert!(matches!(
        maps.read_node(missing).await,
        Err(RevisionMapError::Store(ObjectStoreError::Missing(_)))
    ));
}
