use std::{collections::BTreeMap, sync::Arc};

use object_store::memory::InMemory;
use proptest::prelude::*;
use wheat_core::*;
use wheat_store::*;

/// One of 81 keys of `N` bytes that branch at the first, second, middle and last
/// byte, so updates split, collapse and inline at every depth.
fn key<const N: usize>(index: u8) -> [u8; N] {
    let mut bytes = [7; N];
    for (position, digit) in [0, 1, N / 2, N - 1].into_iter().zip([1, 3, 9, 27]) {
        bytes[position] = index / digit % 3;
    }
    bytes
}

fn digest(value: u8) -> [u8; 32] {
    [value; 32]
}

/// Batches of edits: a key index and the value to set, or `None` to remove it.
fn edit_batches() -> impl Strategy<Value = Vec<Vec<(u8, Option<u8>)>>> {
    prop::collection::vec(
        prop::collection::vec((0..81u8, prop::option::weighted(0.6, 0..4u8)), 1..6),
        1..40,
    )
}

/// The entries left after applying `batches` in order.
fn model(batches: &[Vec<(u8, Option<u8>)>]) -> BTreeMap<u8, u8> {
    let mut model = BTreeMap::new();
    for (index, value) in batches.iter().flatten() {
        match value {
            Some(value) => model.insert(*index, *value),
            None => model.remove(index),
        };
    }
    model
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

proptest! {
    /// Any sequence of inserts and removals through the updater gives the root the
    /// builder makes from the final entries: one encoding per set of entries.
    #[test]
    fn updates_and_removals_match_the_builder(batches in edit_batches()) {
        runtime().block_on(async {
            let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "update-parity");
            let expected = model(&batches);

            let revisions = RevisionMapStore::new(&objects);
            let mut root = revisions.empty_root().await.unwrap();
            for batch in &batches {
                root = revisions
                    .update_batch(
                        root,
                        batch.iter().map(|(index, value)| {
                            (
                                ChangeId::from_bytes(key(*index)),
                                value.map(|value| RevisionId::from_digest(digest(value))),
                            )
                        }),
                    )
                    .await
                    .unwrap();
            }
            let built = build_revision_map(
                expected.iter().map(|(index, value)| {
                    (ChangeId::from_bytes(key(*index)), RevisionId::from_digest(digest(*value)))
                }),
                |_| {},
            );
            assert_eq!(root, built);

            let applications = ApplicationMapStore::new(&objects);
            let mut root = applications.empty_root().await.unwrap();
            for batch in &batches {
                root = applications
                    .update_batch(
                        root,
                        batch.iter().map(|(index, value)| {
                            (
                                ChangeId::from_bytes(key(*index)),
                                value.map(|value| ApplicationId::from_digest(digest(value))),
                            )
                        }),
                    )
                    .await
                    .unwrap();
            }
            let built = build_application_map(
                expected.iter().map(|(index, value)| {
                    (
                        ChangeId::from_bytes(key(*index)),
                        ApplicationId::from_digest(digest(*value)),
                    )
                }),
                |_| {},
            );
            assert_eq!(root, built);

            macro_rules! check_set {
                ($build:ident, $store:ident, $key:ident) => {{
                    let sets = $store::new(&objects);
                    let mut root = sets.empty_root().await.unwrap();
                    for batch in &batches {
                        root = sets
                            .update_batch(
                                root,
                                batch.iter().map(|(index, value)| {
                                    ($key::from_digest(key(*index)), value.is_some())
                                }),
                            )
                            .await
                            .unwrap();
                    }
                    let built = $build(
                        expected.keys().map(|index| $key::from_digest(key(*index))),
                        |_| {},
                    );
                    assert_eq!(root, built);
                }};
            }
            check_set!(build_conflict_set, ConflictSetStore, ConflictId);
            check_set!(build_retained_set, RetainedSetStore, ObjectId);
        });
    }

    /// The change map and the lineage map only grow: inserts in any batching, and the
    /// union of two halves, give the builder's root.
    #[test]
    fn growing_maps_match_the_builder(
        entries in prop::collection::btree_map(0..81u8, 0..4u8, 1..60),
        edges in prop::collection::btree_set((0..81u8, 0..81u8), 1..80),
        batch in 1..8usize,
    ) {
        runtime().block_on(async {
            let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "grow-parity");

            let entries: Vec<_> = entries
                .into_iter()
                .map(|(index, value)| {
                    (
                        ChangeId::from_bytes(key(index)),
                        ChangeObjectId::from_digest(digest(value)),
                    )
                })
                .collect();
            let changes = ChangeMapStore::new(&objects);
            let empty = changes.empty_root().await.unwrap();
            let mut root = empty;
            for &(change, object) in entries.iter().rev() {
                root = changes.insert(root, change, object).await.unwrap();
            }
            let built = build_change_map(entries.iter().copied(), |_| {});
            assert_eq!(root, built);
            let (left, right) = entries.split_at(entries.len() / 2);
            let mut halves = [empty, empty];
            for (half, part) in halves.iter_mut().zip([left, right]) {
                for &(change, object) in part {
                    *half = changes.insert(*half, change, object).await.unwrap();
                }
            }
            assert_eq!(changes.union(halves[0], halves[1]).await.unwrap(), built);

            let edges: Vec<_> = edges
                .into_iter()
                .map(|(parent, child)| {
                    (
                        ViewId::from_digest(key(parent)),
                        ViewId::from_digest(key(child)),
                    )
                })
                .collect();
            let lineage = ViewLineageMapStore::new(&objects);
            let empty = lineage.empty_root().await.unwrap();
            let mut root = empty;
            for chunk in edges.chunks(batch) {
                root = lineage.insert_edges(root, chunk.iter().copied()).await.unwrap();
            }
            let built = build_view_lineage_map(edges.iter().copied(), |_| {});
            assert_eq!(root, built);
            let (left, right) = edges.split_at(edges.len() / 2);
            let left = lineage.insert_edges(empty, left.iter().copied()).await.unwrap();
            let right = lineage.insert_edges(empty, right.iter().copied()).await.unwrap();
            assert_eq!(lineage.union(left, right).await.unwrap(), built);
        });
    }
}

/// A single entry below the root is inlined in its parent. The same entry stored as
/// a terminal child is a second encoding of the same set, and reading it is refused.
#[tokio::test]
async fn a_single_entry_stored_as_a_child_is_refused() {
    let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "forged-terminal");
    let sets = RetainedSetStore::new(&objects);
    let inlined = ObjectId::from_digest([0; 32]);
    let other = ObjectId::from_digest([1; 32]);
    let canonical = build_retained_set([inlined, other], |_| {});

    let terminal_child = sets
        .write_node(RetainedSetNode::new(vec![0; 31], true, Vec::new(), Vec::new()).unwrap())
        .await
        .unwrap();
    let forged = sets
        .write_node(
            RetainedSetNode::new(
                Vec::new(),
                false,
                vec![RetainedSetChild {
                    edge: 0,
                    child: terminal_child,
                }],
                vec![other.as_bytes().to_vec()],
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(forged, canonical);
    assert!(sets.contains(forged, other).await.unwrap());
    assert!(matches!(
        sets.contains(forged, inlined).await,
        Err(CollectionError::InvalidTerminalNode)
    ));
    assert!(matches!(
        sets.read_node_at(terminal_child, 1).await,
        Err(CollectionError::InvalidTerminalNode)
    ));
}

#[tokio::test]
async fn pure_builders_match_incremental_store_roots() {
    let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "builder-parity");
    let entries: Vec<_> = (0..24u8)
        .rev()
        .map(|n| {
            let mut bytes = [42; 16];
            bytes[0] = n % 3;
            bytes[15] = n;
            (
                ChangeId::from_bytes(bytes),
                RevisionId::from_digest([n; 32]),
            )
        })
        .collect();
    let mut emitted = Vec::new();
    let root = build_revision_map(entries.clone(), |object| emitted.push(object));
    let maps = RevisionMapStore::new(&objects);
    let mut incremental = maps.empty_root().await.unwrap();
    for &(key, value) in &entries {
        incremental = maps.insert(incremental, key, value).await.unwrap();
    }
    assert_eq!(root, incremental);
    for object in emitted {
        assert_eq!(
            objects.get(object.id).await.unwrap().canonical_bytes,
            object.bytes
        );
    }

    let entries: Vec<_> = entries
        .into_iter()
        .map(|(key, value)| (key, ApplicationId::from_digest(*value.as_bytes())))
        .collect();
    let root = build_application_map(entries.clone(), |_| {});
    let maps = ApplicationMapStore::new(&objects);
    let incremental = maps
        .update_batch(
            maps.empty_root().await.unwrap(),
            entries.into_iter().map(|(k, v)| (k, Some(v))),
        )
        .await
        .unwrap();
    assert_eq!(root, incremental);

    macro_rules! check_set {
        ($build:ident,$store:ident,$key:ident) => {{
            let keys: Vec<_> = (0..24u8)
                .map(|n| {
                    let mut b = [11; 32];
                    b[0] = n % 3;
                    b[31] = n;
                    $key::from_digest(b)
                })
                .collect();
            let root = $build(keys.clone(), |_| {});
            let sets = $store::new(&objects);
            let empty = sets.empty_root().await.unwrap();
            assert_eq!($build([], |_| {}), empty);
            let incremental = sets
                .update_batch(empty, keys.into_iter().map(|k| (k, true)))
                .await
                .unwrap();
            assert_eq!(root, incremental);
        }};
    }
    check_set!(build_conflict_set, ConflictSetStore, ConflictId);
    // ContentSetStore uses flat sorted blobs rather than a radix trie, so it
    // has no incremental-vs-builder parity to check here.
    check_set!(build_retained_set, RetainedSetStore, ObjectId);
}
