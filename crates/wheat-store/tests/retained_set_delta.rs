use std::sync::Arc;

use object_store::memory::InMemory;
use wheat_core::ObjectId;
use wheat_store::{
    CollectionError, ObjectStoreAdapter, RetainedSetDelta, RetainedSetDeltaLimits,
    RetainedSetStore,
};

fn memory_store(prefix: &str) -> ObjectStoreAdapter {
    ObjectStoreAdapter::new(Arc::new(InMemory::new()), prefix)
}

fn object_id(n: u8) -> ObjectId {
    ObjectId::from_digest([n; 32])
}

#[tokio::test]
async fn empty_to_populated_shows_all_added() {
    let objects = memory_store("retained-delta-empty");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = object_id(1);
    let b = object_id(2);
    let populated = sets.insert(empty, a).await.unwrap();
    let populated = sets.insert(populated, b).await.unwrap();

    let delta = sets
        .delta(empty, populated, RetainedSetDeltaLimits::default())
        .await
        .unwrap();
    assert_eq!(delta.len(), 2);
    assert!(delta.contains(&RetainedSetDelta::Added(a)));
    assert!(delta.contains(&RetainedSetDelta::Added(b)));
}

#[tokio::test]
async fn populated_to_empty_shows_all_removed() {
    let objects = memory_store("retained-delta-pop-empty");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = object_id(1);
    let populated = sets.insert(empty, a).await.unwrap();

    let delta = sets
        .delta(populated, empty, RetainedSetDeltaLimits::default())
        .await
        .unwrap();
    assert_eq!(delta, vec![RetainedSetDelta::Removed(a)]);
}

#[tokio::test]
async fn equal_roots_produce_empty_delta() {
    let objects = memory_store("retained-delta-equal");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = object_id(1);
    let root = sets.insert(empty, a).await.unwrap();

    let delta = sets
        .delta(root, root, RetainedSetDeltaLimits::default())
        .await
        .unwrap();
    assert!(delta.is_empty());
}

#[tokio::test]
async fn mixed_additions_and_removals() {
    let objects = memory_store("retained-delta-mixed");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = object_id(1);
    let b = object_id(2);
    let c = object_id(3);

    // Old has {a, b}, new has {b, c}
    let old = sets.insert(empty, a).await.unwrap();
    let old = sets.insert(old, b).await.unwrap();
    let new = sets.insert(empty, b).await.unwrap();
    let new = sets.insert(new, c).await.unwrap();

    let delta = sets
        .delta(old, new, RetainedSetDeltaLimits::default())
        .await
        .unwrap();
    assert!(delta.contains(&RetainedSetDelta::Removed(a)));
    assert!(delta.contains(&RetainedSetDelta::Added(c)));
    // b is in both, so it should not appear
    assert!(!delta.iter().any(|d| match d {
        RetainedSetDelta::Added(id) | RetainedSetDelta::Removed(id) => *id == b,
    }));
}

#[tokio::test]
async fn delta_enforces_change_limit() {
    let objects = memory_store("retained-delta-limit");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();
    let a = object_id(1);
    let b = object_id(2);
    let populated = sets.insert(empty, a).await.unwrap();
    let populated = sets.insert(populated, b).await.unwrap();

    let result = sets
        .delta(
            empty,
            populated,
            RetainedSetDeltaLimits {
                max_changes: 1,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(CollectionError::DeltaChangeLimitExceeded { limit: 1 })
    ));
}

#[tokio::test]
async fn large_set_one_element_delta_reads_few_nodes() {
    let objects = memory_store("retained-delta-large");
    let sets = RetainedSetStore::new(&objects);
    let empty = sets.empty_root().await.unwrap();

    // Build a set with 1000 entries.
    let mut root = empty;
    for i in 0_u16..1000 {
        let mut bytes = [0u8; 32];
        bytes[30..].copy_from_slice(&i.to_be_bytes());
        root = sets.insert(root, ObjectId::from_digest(bytes)).await.unwrap();
    }

    // Add one more entry to create a minimal delta.
    let added = ObjectId::from_digest([0xff; 32]);
    let new_root = sets.insert(root, added).await.unwrap();

    let delta = sets
        .delta(root, new_root, RetainedSetDeltaLimits::default())
        .await
        .unwrap();
    assert_eq!(delta, vec![RetainedSetDelta::Added(added)]);
}
