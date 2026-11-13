//! Pool GC: a pool that has been published to twice holds the first publish's
//! envelope and label manifest, and nothing reaches them. The dry run counts them;
//! the collection removes them.

use std::path::Path;

use crate::support;

use wheat_core::{ApplicationContext, ObjectId, Repository, RepositoryEnvelopeId, Tree};
use wheat_store::{
    CollectableStructure, ContentStore, ImmutableObjectStore, LocalPublicationStore,
    ObjectStoreError, PackedObjectStore, PinnedObjects, RepositoryPublisher, RepositoryReader,
    RepositoryStore, SnapshotLimits, StoredInventory, StoredRecord, StructureCollectionError,
    analyze_structure, collect_structure,
};

async fn pool() -> (
    tempfile::TempDir,
    PackedObjectStore,
    RepositoryEnvelopeId,
    RepositoryEnvelopeId,
) {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    publish_twice(dir, objects, &content).await
}

/// A pool whose objects all live in files of their own, and whose content it keeps
/// beside them, as a single repository's store does.
async fn loose_pool() -> (
    tempfile::TempDir,
    PackedObjectStore,
    RepositoryEnvelopeId,
    RepositoryEnvelopeId,
) {
    let dir = tempfile::tempdir().unwrap();
    let objects =
        PackedObjectStore::open_with_loose_threshold(dir.path().join("storage"), 0).unwrap();
    let content = objects.content_store();
    publish_twice(dir, objects, &content).await
}

async fn publish_twice(
    dir: tempfile::TempDir,
    objects: PackedObjectStore,
    content: &ContentStore,
) -> (
    tempfile::TempDir,
    PackedObjectStore,
    RepositoryEnvelopeId,
    RepositoryEnvelopeId,
) {
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let publisher = RepositoryPublisher::new(&objects, content, &heads);
    let first = publisher.initialize(&repo).await.unwrap();
    let id = content.put(b"hello").unwrap();
    let bytes = content.get(id).unwrap();
    repo.create_change(
        base,
        base,
        [],
        support::exact("a", None, &bytes),
        ApplicationContext::new(content),
    )
    .unwrap();
    let second = publisher.publish(first, &repo).await.unwrap();
    (dir, objects, first.envelope, second.envelope)
}

#[tokio::test]
async fn superseded_publishes_are_counted_unreachable() {
    let (_dir, objects, first, second) = pool().await;

    let report = analyze_structure(&objects, &objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert_eq!(report.roots, 1);
    assert_eq!(report.unlisted, 0);
    assert!(report.total_reachable().records > 0);
    let unreachable = report.total_unreachable();
    assert!(
        unreachable.records >= 2,
        "the old envelope and its manifest"
    );
    assert!(unreachable.bytes > 0);

    // Keeping the first head as a root reaches everything that was only its own.
    let both = analyze_structure(
        &objects,
        &objects,
        &[first, second],
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(both.total_unreachable().records, 0);
    assert_eq!(
        both.total_reachable().records,
        report.total_reachable().records + unreachable.records
    );
}

#[tokio::test]
async fn the_inventory_lists_each_object_once() {
    let (_dir, objects, _first, second) = pool().await;
    let mut listed = Vec::new();
    objects
        .for_each_record(&mut |record| {
            if let StoredRecord::Object { id, .. } = record {
                listed.push(id);
            }
        })
        .unwrap();
    let total = listed.len();
    listed.sort();
    listed.dedup();
    assert_eq!(listed.len(), total);
    let report = analyze_structure(&objects, &objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert_eq!(
        report.total_reachable().records + report.total_unreachable().records,
        total as u64
    );
}
fn missing(result: Result<wheat_store::StoredObject, ObjectStoreError>) -> bool {
    matches!(result, Err(ObjectStoreError::Missing(_)))
}

/// The names of the files directly in `directory`, sorted.
fn files_in(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Every file under `directory`, at any depth.
fn count_files(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .map(|entry| {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                count_files(&entry.path())
            } else {
                1
            }
        })
        .sum()
}

#[tokio::test]
async fn a_collection_removes_what_no_head_reaches() {
    let (dir, objects, first, second) = pool().await;
    let content = ContentStore::new(dir.path().join("content"));
    let before = analyze_structure(&objects, &objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();

    // The pool's own content store does not hold the repository's content, which a
    // collection's validation reads; give it the one that does.
    let store = wheat_store::NetworkStore::new(
        objects.clone(),
        wheat_store::SeparateRepositoryStore::new(objects.clone(), content.clone()),
    );
    let collection = collect_structure(&store, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert_eq!(collection.roots, 1);
    assert_eq!(collection.marked, before.total_reachable().records);
    assert_eq!(
        collection.sweep.objects_dropped,
        before.total_unreachable().records
    );
    assert_eq!(
        collection.sweep.object_bytes_dropped,
        before.total_unreachable().bytes
    );
    assert_eq!(
        collection.sweep.objects_kept,
        before.total_reachable().records
    );
    assert!(collection.sweep.segment_bytes_after < collection.sweep.segment_bytes_before);

    assert!(missing(objects.get(ObjectId::from(first)).await));
    let after = analyze_structure(&objects, &objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert_eq!(after.total_unreachable().records, 0);
    assert_eq!(after.total_reachable(), before.total_reachable());
    RepositoryReader::new(&objects, &content)
        .validate_envelope(second)
        .await
        .unwrap();

    // The old segments are gone once nothing pins them, and the collection holds
    // after the store is opened again.
    drop(store);
    let segments = dir.path().join("storage/packed/segments");
    assert_eq!(
        files_in(&segments).len() as u64,
        collection.sweep.segments_after
    );
    objects.close().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    assert!(missing(objects.get(ObjectId::from(first)).await));
    RepositoryReader::new(&objects, &content)
        .validate_envelope(second)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_pin_reads_what_a_later_sweep_removed_until_it_is_dropped() {
    let (dir, objects, first, second) = pool().await;
    let content = ContentStore::new(dir.path().join("content"));
    let store = wheat_store::NetworkStore::new(
        objects.clone(),
        wheat_store::SeparateRepositoryStore::new(objects.clone(), content.clone()),
    );
    let segments = dir.path().join("storage/packed/segments");
    let pin = objects.pin();
    let unpinned_before = objects.pin();
    drop(unpinned_before);

    let collection = collect_structure(&store, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    let pinned = PinnedObjects::new(&objects, Some(pin));
    let envelope = pinned.get(ObjectId::from(first)).await.unwrap();
    assert_eq!(envelope.id, ObjectId::from(first));
    assert_eq!(
        pinned.exists_many(&[ObjectId::from(first)]).await.unwrap(),
        [true]
    );
    // A pin taken after the sweep does not see what it removed.
    let later = PinnedObjects::new(&objects, Some(objects.pin()));
    assert!(missing(later.get(ObjectId::from(first)).await));
    assert!(missing(objects.get(ObjectId::from(first)).await));
    let pinned_files = files_in(&segments).len() as u64;
    assert_eq!(
        pinned_files,
        collection.sweep.segments_after + collection.sweep.segments_before
    );

    drop(pinned);
    assert_eq!(
        files_in(&segments).len() as u64,
        collection.sweep.segments_after
    );
}

#[tokio::test]
async fn a_sweep_retires_loose_objects_and_keeps_the_pools_content() {
    let (dir, objects, first, second) = loose_pool().await;
    let retired = dir.path().join("storage/retired");
    let loose = dir.path().join("storage/loose-objects");
    let loose_before = count_files(&loose);
    let pin = objects.pin();

    let collection = collect_structure(&objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert!(collection.sweep.loose_objects_retired >= 2);
    assert!(collection.sweep.loose_object_bytes_retired > 0);
    assert_eq!(
        count_files(&loose),
        loose_before - collection.sweep.loose_objects_retired as usize
    );
    assert_eq!(
        count_files(&retired),
        collection.sweep.loose_objects_retired as usize
    );
    assert!(missing(objects.get(ObjectId::from(first)).await));
    let pinned = PinnedObjects::new(&objects, Some(pin));
    pinned.get(ObjectId::from(first)).await.unwrap();
    let content = objects.content_store();
    RepositoryReader::new(&objects, &content)
        .validate_envelope(second)
        .await
        .unwrap();

    drop(pinned);
    assert_eq!(count_files(&retired), 0);
    let report = analyze_structure(&objects, &objects, &[second], SnapshotLimits::default())
        .await
        .unwrap();
    assert_eq!(report.total_unreachable().records, 0);
    assert_eq!(
        report.total_reachable().records,
        collection.marked,
        "every marked object is still there"
    );
}

#[tokio::test]
async fn a_collection_fails_closed() {
    let (dir, objects, first, second) = pool().await;
    let content = ContentStore::new(dir.path().join("content"));
    let store = wheat_store::NetworkStore::new(
        objects.clone(),
        wheat_store::SeparateRepositoryStore::new(objects.clone(), content.clone()),
    );
    assert!(matches!(
        collect_structure(&store, &[], SnapshotLimits::default()).await,
        Err(StructureCollectionError::NoHeads)
    ));
    let absent = RepositoryEnvelopeId::from_digest([7; 32]);
    assert!(matches!(
        collect_structure(&store, &[second, absent], SnapshotLimits::default()).await,
        Err(StructureCollectionError::Invalid { head, .. }) if head == absent
    ));
    // A store whose content is not the repository's cannot validate a head that
    // names content.
    assert!(matches!(
        collect_structure(&objects, &[second], SnapshotLimits::default()).await,
        Err(StructureCollectionError::Invalid { head, .. }) if head == second
    ));
    // A store that cannot collect is refused before anything is read.
    let separate = wheat_store::SeparateRepositoryStore::new(objects.clone(), content.clone());
    assert!(separate.collectable_structure().is_none());
    assert!(matches!(
        collect_structure(&separate, &[second], SnapshotLimits::default()).await,
        Err(StructureCollectionError::Unsupported)
    ));
    objects.get(ObjectId::from(first)).await.unwrap();
}
/// A process that stops while a pin holds what a sweep removed leaves the files
/// behind; opening the store removes them.
#[tokio::test]
async fn opening_removes_what_a_sweep_left_behind() {
    for loose in [false, true] {
        let (dir, objects, first, second) = if loose {
            loose_pool().await
        } else {
            pool().await
        };
        let content = if loose {
            objects.content_store()
        } else {
            ContentStore::new(dir.path().join("content"))
        };
        let store = wheat_store::NetworkStore::new(
            objects.clone(),
            wheat_store::SeparateRepositoryStore::new(objects.clone(), content.clone()),
        );
        let pin = objects.pin();
        let collection = collect_structure(&store, &[second], SnapshotLimits::default())
            .await
            .unwrap();
        drop((store, content));
        objects.close().unwrap();
        let segments = dir.path().join("storage/packed/segments");
        let retired = dir.path().join("storage/retired");
        assert_eq!(
            files_in(&segments).len() as u64,
            collection.sweep.segments_before + collection.sweep.segments_after,
            "loose: {loose}"
        );
        assert_eq!(
            count_files(&retired) as u64,
            collection.sweep.loose_objects_retired
        );

        let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
        assert_eq!(
            files_in(&segments).len() as u64,
            collection.sweep.segments_after,
            "loose: {loose}"
        );
        assert!(!retired.exists());
        assert!(missing(objects.get(ObjectId::from(first)).await));
        objects.get(ObjectId::from(second)).await.unwrap();
        drop(pin);
    }
}
