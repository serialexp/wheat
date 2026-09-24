use std::{collections::BTreeSet, sync::Arc};

use object_store::memory::InMemory;
use wheat_core::{ApplicationId, ApplicationMapId, ChangeId, ConflictId, ContentId, ObjectId};
use wheat_store::{
    ApplicationMapStore, ConflictSetStore, ContentSetDelta, ContentSetStore, ImmutableObjectStore,
    LooseObjectStore, ObjectStoreAdapter, RetainedSetStore, RevisionMapStore,
};

async fn application_conformance<S: ImmutableObjectStore>(objects: &S) {
    let maps = ApplicationMapStore::new(objects);
    let empty = maps.empty_root().await.unwrap();
    let a = ChangeId::from_bytes([1; 16]);
    let b = ChangeId::from_bytes([255; 16]);
    let first = ApplicationId::from_digest([3; 32]);
    let last = ApplicationId::from_digest([9; 32]);
    let root = maps
        .update_batch(empty, [(b, Some(first)), (a, Some(first)), (a, Some(last))])
        .await
        .unwrap();
    assert_eq!(maps.get(root, a).await.unwrap(), Some(last));
    assert_eq!(maps.get(empty, a).await.unwrap(), None);
    let mut cursor = maps.cursor(root).await.unwrap();
    assert_eq!(cursor.next().await.unwrap(), Some((a, last)));
    assert_eq!(cursor.next().await.unwrap(), Some((b, first)));
    assert_eq!(cursor.next().await.unwrap(), None);
    assert_eq!(maps.insert(root, a, last).await.unwrap(), root);
    let removed = maps.remove(root, a).await.unwrap();
    assert_eq!(maps.get(removed, a).await.unwrap(), None);
    assert_eq!(maps.get(root, a).await.unwrap(), Some(last));
    assert_eq!(maps.remove(removed, b).await.unwrap(), empty);

    let revision_root = RevisionMapStore::new(objects).empty_root().await.unwrap();
    let forged = ApplicationMapId::from_digest(*revision_root.as_bytes());
    assert!(
        maps.cursor(forged).await.is_err(),
        "matching shape does not permit another family"
    );
}

macro_rules! set_conformance {
    ($function:ident, $store:ident, $key:ident) => {
        async fn $function<S: ImmutableObjectStore>(objects: &S) {
            let sets = $store::new(objects);
            let empty = sets.empty_root().await.unwrap();
            let mut root = empty;
            let mut expected = BTreeSet::new();
            let mut state = 0x1234_5678_9abc_def0_u64;
            for _ in 0..12 {
                let mut edits = Vec::with_capacity(24);
                for _ in 0..24 {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let slot = (state >> 32) as u8 % 31;
                    let mut bytes = [0x66; 32];
                    bytes[0] = slot % 3;
                    bytes[15] = slot % 7;
                    bytes[31] = slot;
                    let key = $key::from_digest(bytes);
                    let present = state & 3 != 0;
                    edits.push((key, present));
                    if present {
                        expected.insert(key);
                    } else {
                        expected.remove(&key);
                    }
                }
                let old = root;
                root = sets.update_batch(root, edits.clone()).await.unwrap();
                assert_eq!(sets.update_batch(root, edits).await.unwrap(), root);
                let mut cursor = sets.cursor(root).await.unwrap();
                let mut actual = BTreeSet::new();
                let mut previous = None;
                while let Some(key) = cursor.next().await.unwrap() {
                    assert!(previous.is_none_or(|previous| previous < key));
                    previous = Some(key);
                    actual.insert(key);
                    assert!(sets.contains(root, key).await.unwrap());
                }
                assert_eq!(actual, expected);
                assert_eq!(cursor.next().await.unwrap(), None);
                // Historical roots remain traversable after every mutation.
                let mut old_cursor = sets.cursor(old).await.unwrap();
                while old_cursor.next().await.unwrap().is_some() {}
            }
            let marker = $key::from_digest([0xfe; 32]);
            let singleton = sets.insert(empty, marker).await.unwrap();
            assert!(!sets.contains(root, marker).await.unwrap());
            let union = sets.union(root, singleton).await.unwrap();
            assert!(sets.contains(union, marker).await.unwrap());
            let difference = sets.difference(union, singleton).await.unwrap();
            let mut difference_cursor = sets.cursor(difference).await.unwrap();
            let mut root_cursor = sets.cursor(root).await.unwrap();
            loop {
                let difference_member = difference_cursor.next().await.unwrap();
                let root_member = root_cursor.next().await.unwrap();
                assert_eq!(difference_member, root_member);
                if difference_member.is_none() {
                    break;
                }
            }
            assert_eq!(sets.union(root, root).await.unwrap(), root);
            assert_eq!(sets.union(root, singleton).await.unwrap(), union);
            assert_eq!(sets.union(singleton, root).await.unwrap(), union);

            // Differential structural-union test: compare every member with an ordinary COW
            // update and verify commutativity/idempotence at the canonical root level.
            let left_keys: Vec<_> = (0..40)
                .map(|slot| {
                    let mut bytes = [0_u8; 32];
                    bytes[0] = slot % 2;
                    bytes[1] = slot;
                    $key::from_digest(bytes)
                })
                .collect();
            let right_keys: Vec<_> = (20..60)
                .map(|slot| {
                    let mut bytes = [0_u8; 32];
                    bytes[0] = slot % 2;
                    bytes[1] = slot;
                    $key::from_digest(bytes)
                })
                .collect();
            let left = sets
                .update_batch(empty, left_keys.iter().copied().map(|key| (key, true)))
                .await
                .unwrap();
            let right = sets
                .update_batch(empty, right_keys.iter().copied().map(|key| (key, true)))
                .await
                .unwrap();
            let structural = sets.union(left, right).await.unwrap();
            let expected = sets
                .update_batch(left, right_keys.iter().copied().map(|key| (key, true)))
                .await
                .unwrap();
            let mut structural_cursor = sets.cursor(structural).await.unwrap();
            let mut expected_cursor = sets.cursor(expected).await.unwrap();
            loop {
                let structural_member = structural_cursor.next().await.unwrap();
                let expected_member = expected_cursor.next().await.unwrap();
                assert_eq!(structural_member, expected_member);
                if structural_member.is_none() {
                    break;
                }
            }
            assert_eq!(sets.union(right, left).await.unwrap(), structural);
            assert_eq!(
                sets.union(structural, structural).await.unwrap(),
                structural
            );
            for key in left_keys.iter().chain(&right_keys) {
                assert!(sets.contains(structural, *key).await.unwrap());
            }
            assert_eq!(sets.difference(root, root).await.unwrap(), empty);
            assert_eq!(sets.remove(singleton, marker).await.unwrap(), empty);
        }
    };
}

set_conformance!(conflict_conformance, ConflictSetStore, ConflictId);
set_conformance!(content_conformance, ContentSetStore, ContentId);
set_conformance!(retained_conformance, RetainedSetStore, ObjectId);

#[tokio::test]
async fn content_set_delta_is_sorted_and_prunes_equal_roots() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let contents = ContentSetStore::new(&objects);
    let empty = contents.empty_root().await.unwrap();
    let a = ContentId::from_digest([1; 32]);
    let b = ContentId::from_digest([2; 32]);
    let c = ContentId::from_digest([3; 32]);
    let old = contents
        .update_batch(empty, [(a, true), (b, true)])
        .await
        .unwrap();
    let new = contents
        .update_batch(old, [(a, false), (c, true)])
        .await
        .unwrap();
    assert_eq!(
        contents.delta(old, new, Default::default()).await.unwrap(),
        vec![ContentSetDelta::Removed(a), ContentSetDelta::Added(c)]
    );
    assert!(
        contents
            .delta(new, new, Default::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn memory_backend_supports_all_typed_collections() {
    let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "typed-collections");
    application_conformance(&objects).await;
    conflict_conformance(&objects).await;
    content_conformance(&objects).await;
    retained_conformance(&objects).await;
    let conflict = ConflictSetStore::new(&objects).empty_root().await.unwrap();
    let content = ContentSetStore::new(&objects).empty_root().await.unwrap();
    let retained = RetainedSetStore::new(&objects).empty_root().await.unwrap();
    assert_ne!(conflict.as_bytes(), content.as_bytes());
    assert_ne!(content.as_bytes(), retained.as_bytes());
    assert_ne!(conflict.as_bytes(), retained.as_bytes());
}

#[tokio::test]
async fn loose_backend_supports_all_typed_collections() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    application_conformance(&objects).await;
    conflict_conformance(&objects).await;
    content_conformance(&objects).await;
    retained_conformance(&objects).await;
}

#[tokio::test]
async fn typed_roots_reopen_without_hydrated_state() {
    let directory = tempfile::tempdir().unwrap();
    let change = ChangeId::from_bytes([7; 16]);
    let application = ApplicationId::from_digest([8; 32]);
    let conflict = ConflictId::from_digest([9; 32]);
    let content = ContentId::of(b"historical edit content");
    let retained = ObjectId::from_digest([11; 32]);
    let (a, b, c, d) = {
        let objects = LooseObjectStore::new(directory.path());
        let maps = ApplicationMapStore::new(&objects);
        let conflicts = ConflictSetStore::new(&objects);
        let contents = ContentSetStore::new(&objects);
        let objects_set = RetainedSetStore::new(&objects);
        (
            maps.insert(maps.empty_root().await.unwrap(), change, application)
                .await
                .unwrap(),
            conflicts
                .insert(conflicts.empty_root().await.unwrap(), conflict)
                .await
                .unwrap(),
            contents
                .insert(contents.empty_root().await.unwrap(), content)
                .await
                .unwrap(),
            objects_set
                .insert(objects_set.empty_root().await.unwrap(), retained)
                .await
                .unwrap(),
        )
    };
    let objects = LooseObjectStore::new(directory.path());
    assert_eq!(
        ApplicationMapStore::new(&objects)
            .get(a, change)
            .await
            .unwrap(),
        Some(application)
    );
    assert!(
        ConflictSetStore::new(&objects)
            .contains(b, conflict)
            .await
            .unwrap()
    );
    assert!(
        ContentSetStore::new(&objects)
            .contains(c, content)
            .await
            .unwrap()
    );
    assert!(
        RetainedSetStore::new(&objects)
            .contains(d, retained)
            .await
            .unwrap()
    );
}
