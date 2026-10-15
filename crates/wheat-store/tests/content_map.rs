use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::*;
use wheat_store::*;

fn blob(index: u32) -> ContentId {
    ContentId::of(&index.to_le_bytes())
}

struct CountsReads {
    inner: ObjectStoreAdapter,
    reads: AtomicUsize,
}

#[async_trait]
impl ImmutableObjectStore for CountsReads {
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

fn store(name: &str) -> CountsReads {
    CountsReads {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), name),
        reads: AtomicUsize::new(0),
    }
}

/// Two paths naming one blob count it twice; losing one path keeps it, losing both
/// removes it, and the result is always the builder's root for the same counts.
#[tokio::test]
async fn adjust_counts_each_tree_entry_and_drops_a_blob_at_zero() {
    let objects = store("adjust");
    let maps = ContentMapStore::new(&objects);
    let empty = maps.empty_root().await.unwrap();
    let (shared, other) = (blob(1), blob(2));

    let both = maps
        .adjust(empty, [(shared, 1), (shared, 1), (other, 1)])
        .await
        .unwrap();
    assert_eq!(maps.get(both, shared).await.unwrap(), Some(2));
    assert_eq!(both, build_content_map([(shared, 2), (other, 1)], |_| {}));

    let one = maps.adjust(both, [(shared, -1)]).await.unwrap();
    assert_eq!(maps.get(one, shared).await.unwrap(), Some(1));
    assert_eq!(one, build_content_map([(shared, 1), (other, 1)], |_| {}));

    // A path moving from one blob to the other: -1 and +1 in one call.
    let moved = maps.adjust(one, [(shared, -1), (other, 1)]).await.unwrap();
    assert_eq!(maps.get(moved, shared).await.unwrap(), None);
    assert_eq!(moved, build_content_map([(other, 2)], |_| {}));

    // Changes that cancel leave the map as it is.
    assert_eq!(
        maps.adjust(moved, [(shared, 1), (shared, -1)])
            .await
            .unwrap(),
        moved
    );
}

#[tokio::test]
async fn adjust_refuses_to_count_below_zero() {
    let objects = store("underflow");
    let maps = ContentMapStore::new(&objects);
    let root = maps
        .adjust(maps.empty_root().await.unwrap(), [(blob(1), 1)])
        .await
        .unwrap();
    assert!(matches!(
        maps.adjust(root, [(blob(1), -2)]).await,
        Err(ContentMapError::CountUnderflow { content }) if content == blob(1)
    ));
    assert!(matches!(
        maps.adjust(root, [(blob(9), -1)]).await,
        Err(ContentMapError::CountUnderflow { content }) if content == blob(9)
    ));
}

/// `added` names the blobs only the new map has, in key order, and `delta` reports
/// count changes too. Neither reads the subtrees the maps share.
#[tokio::test]
async fn added_and_delta_follow_the_difference() {
    let objects = store("added");
    let maps = ContentMapStore::new(&objects);
    let base = maps
        .adjust(
            maps.empty_root().await.unwrap(),
            (0..4096).map(|index| (blob(index), 1)),
        )
        .await
        .unwrap();
    let next = maps
        .adjust(
            base,
            [
                (blob(5000), 1),
                (blob(5001), 1),
                (blob(7), 1),
                (blob(8), -1),
            ],
        )
        .await
        .unwrap();

    objects.reads.store(0, Ordering::SeqCst);
    let mut expected = vec![blob(5000), blob(5001)];
    expected.sort();
    assert_eq!(maps.added(base, next).await.unwrap(), expected);
    assert!(maps.added(next, next).await.unwrap().is_empty());
    let reads = objects.reads.load(Ordering::SeqCst);
    // Four changed paths of a 4096-key map, two maps each: the roots and the nodes
    // on those paths, not the whole map.
    assert!(reads <= 4 * 2 * 3, "added read {reads} nodes");

    let mut changes = maps
        .delta(base, next, ContentMapDeltaLimits::default())
        .await
        .unwrap();
    let mut expected = vec![
        ContentMapDelta::Added {
            content: blob(5000),
            count: 1,
        },
        ContentMapDelta::Added {
            content: blob(5001),
            count: 1,
        },
        ContentMapDelta::Changed {
            content: blob(7),
            old: 1,
            new: 2,
        },
        ContentMapDelta::Removed {
            content: blob(8),
            count: 1,
        },
    ];
    let key = |delta: &ContentMapDelta| match *delta {
        ContentMapDelta::Added { content, .. }
        | ContentMapDelta::Changed { content, .. }
        | ContentMapDelta::Removed { content, .. } => content,
    };
    expected.sort_by_key(key);
    assert_eq!(changes, expected);
    changes = maps
        .delta(next, base, ContentMapDeltaLimits::default())
        .await
        .unwrap();
    assert_eq!(changes.len(), 4);
}
