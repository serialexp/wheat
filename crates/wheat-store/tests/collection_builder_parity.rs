use std::sync::Arc;

use object_store::memory::InMemory;
use wheat_core::*;
use wheat_store::*;

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
    // ContentSetStore now uses flat sorted blobs, not radix tries, so parity
    // with `build_content_set` (which produces radix nodes) no longer applies.
    check_set!(build_retained_set, RetainedSetStore, ObjectId);
}
