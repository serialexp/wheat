use std::{sync::Arc, time::Instant};

use object_store::memory::InMemory;
use wheat_core::{ContentId, ContentSetBlob, ContentSetId};
use wheat_store::{
    CollectionError, ContentSetStore, ImmutableObjectStore, ObjectStoreAdapter, ObjectStoreError,
};

fn content_id(value: u64) -> ContentId {
    let mut bytes = [0; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    ContentId::from_digest(bytes)
}

fn store(name: &str) -> ObjectStoreAdapter {
    ObjectStoreAdapter::new(Arc::new(InMemory::new()), name)
}

async fn blob(
    objects: &ObjectStoreAdapter,
    id: ContentSetId,
) -> Result<ContentSetBlob, ObjectStoreError> {
    let stored = objects.get(id.into()).await?;
    Ok(wheat_core::decode_content_set_blob(&stored.canonical_bytes).unwrap())
}

#[tokio::test]
async fn ordinary_updates_choose_delta_then_roll_over_to_snapshots() {
    let objects = store("content-set-rollover");
    let sets = ContentSetStore::new(&objects);
    let basis: Vec<_> = (0..2_000).map(content_id).collect();
    let mut root = sets.write_snapshot(&basis).await.unwrap();

    for update in 0_u64..34 {
        root = sets
            .update_batch_compressing(root, [(content_id(10_000 + update), true)])
            .await
            .unwrap();
        let depth = sets.chain_depth(root).await.unwrap();
        assert_eq!(depth, ((update + 1) % 16) as usize);
        assert_eq!(
            matches!(
                blob(&objects, root).await.unwrap(),
                ContentSetBlob::Snapshot(_)
            ),
            (update + 1) % 16 == 0
        );
    }

    assert_eq!(sets.resolve(root).await.unwrap().len(), basis.len() + 34);
}

/// `update_batch` must stay membership-deterministic: identical membership via
/// different update histories must land on the same canonical root, because
/// repository manifests and hydrated-state closure IDs derive from it.
#[tokio::test]
async fn plain_updates_are_membership_deterministic_regardless_of_history() {
    let objects = store("content-set-deterministic");
    let sets = ContentSetStore::new(&objects);
    let basis: Vec<_> = (0..2_000).map(content_id).collect();
    let extra = content_id(99_999);

    let direct = sets
        .update_batch(sets.write_snapshot(&basis).await.unwrap(), [(extra, true)])
        .await
        .unwrap();

    // The same final membership, reached through many small updates that would
    // each be delta-eligible under the compressing policy.
    let mut stepped = sets.write_snapshot(&basis).await.unwrap();
    for id in &basis[basis.len() - 20..] {
        stepped = sets.update_batch(stepped, [(*id, false)]).await.unwrap();
        stepped = sets.update_batch(stepped, [(*id, true)]).await.unwrap();
    }
    stepped = sets.update_batch(stepped, [(extra, true)]).await.unwrap();
    for id in &basis[basis.len() - 20..] {
        stepped = sets.update_batch(stepped, [(*id, false)]).await.unwrap();
        stepped = sets.update_batch(stepped, [(*id, true)]).await.unwrap();
    }

    assert_eq!(
        sets.resolve(direct).await.unwrap(),
        sets.resolve(stepped).await.unwrap()
    );
    assert_eq!(direct, stepped, "membership alone must determine the root");
}

#[tokio::test]
async fn removals_and_duplicate_edits_are_last_write_wins() {
    let objects = store("content-set-removals");
    let sets = ContentSetStore::new(&objects);
    let initial: Vec<_> = (0..1_000).map(content_id).collect();
    let root = sets.write_snapshot(&initial).await.unwrap();
    let changed = sets
        .update_batch(
            root,
            [
                (content_id(5), false),
                (content_id(5), true),
                (content_id(5), false),
                (content_id(6), false),
                (content_id(2_000), false),
                (content_id(2_001), true),
                (content_id(2_001), false),
                (content_id(2_001), true),
            ],
        )
        .await
        .unwrap();

    assert!(!sets.contains(changed, content_id(5)).await.unwrap());
    assert!(!sets.contains(changed, content_id(6)).await.unwrap());
    assert!(sets.contains(changed, content_id(2_001)).await.unwrap());
    let no_op = sets
        .update_batch(changed, [(content_id(5), true), (content_id(5), false)])
        .await
        .unwrap();
    assert_eq!(no_op, changed);
}

#[tokio::test]
async fn large_replacement_falls_back_to_smaller_snapshot() {
    let objects = store("content-set-replacement");
    let sets = ContentSetStore::new(&objects);
    let old: Vec<_> = (0..2_000).map(content_id).collect();
    let root = sets.write_snapshot(&old).await.unwrap();
    let edits = old
        .iter()
        .copied()
        .map(|id| (id, false))
        .chain((3_000..5_000).map(|value| (content_id(value), true)));
    let replaced = sets.update_batch(root, edits).await.unwrap();

    assert!(matches!(
        blob(&objects, replaced).await.unwrap(),
        ContentSetBlob::Snapshot(_)
    ));
    assert_eq!(sets.chain_depth(replaced).await.unwrap(), 0);
    assert_eq!(
        sets.resolve(replaced).await.unwrap(),
        (3_000..5_000).map(content_id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn missing_bad_and_overdepth_chains_are_rejected() {
    let objects = store("content-set-invalid");
    let sets = ContentSetStore::new(&objects);
    let missing = ContentSetId::from_digest([0x99; 32]);
    assert!(matches!(
        sets.resolve(missing).await,
        Err(CollectionError::Store(ObjectStoreError::Missing(_)))
    ));

    let empty = sets.empty_root().await.unwrap();
    let wrong_kind = content_id(1);
    assert!(matches!(
        sets.resolve(ContentSetId::from_digest(*wrong_kind.as_bytes()))
            .await,
        Err(CollectionError::Store(ObjectStoreError::Missing(_)))
    ));

    let mut overdeep = empty;
    for value in 0..17 {
        overdeep = sets
            .write_delta(overdeep, &[content_id(100 + value)], &[])
            .await
            .unwrap();
    }
    assert!(matches!(
        sets.resolve(overdeep).await,
        Err(CollectionError::ContentSetChainTooDeep { limit: 16 })
    ));
}

#[tokio::test]
async fn aggregate_chain_growth_is_bounded_before_member_allocation() {
    let objects = store("content-set-aggregate-limit");
    let sets = ContentSetStore::new(&objects);
    let too_many: Vec<_> = (0..=1_000_000_u64).map(content_id).collect();
    let bytes = wheat_core::content_set_snapshot_bytes(&too_many);
    let id = ContentSetId::of_canonical_bytes(&bytes);
    objects.put_if_absent(id.into(), &bytes).await.unwrap();

    assert!(matches!(
        sets.resolve(id).await,
        Err(CollectionError::ContentSetByteLimitExceeded { .. })
            | Err(CollectionError::ContentSetMemberLimitExceeded { .. })
    ));
}

#[tokio::test]
async fn equivalent_batches_are_deterministic() {
    let objects = store("content-set-determinism");
    let sets = ContentSetStore::new(&objects);
    let basis: Vec<_> = (0..1_000).map(content_id).collect();
    let root = sets.write_snapshot(&basis).await.unwrap();
    let a = sets
        .update_batch(
            root,
            [
                (content_id(7), false),
                (content_id(2_000), true),
                (content_id(8), false),
            ],
        )
        .await
        .unwrap();
    let b = sets
        .update_batch(
            root,
            [
                (content_id(8), false),
                (content_id(2_000), true),
                (content_id(7), false),
            ],
        )
        .await
        .unwrap();
    assert_eq!(a, b);
}

#[tokio::test]
async fn representative_scale_update_is_linear_enough_for_interactive_use() {
    let objects = store("content-set-scale");
    let sets = ContentSetStore::new(&objects);
    let members: Vec<_> = (0..100_000).map(content_id).collect();
    let root = sets.write_snapshot(&members).await.unwrap();
    let started = Instant::now();
    let changed = sets
        .update_batch(
            root,
            (0..10_000).map(|value| (content_id(value * 3), false)),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(sets.resolve(changed).await.unwrap().len(), 90_000);
    assert!(
        elapsed.as_secs_f32() < 10.0,
        "100k-member/10k-edit update took {elapsed:?}"
    );
}
