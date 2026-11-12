//! The dry run of pool GC: a pool that has been published to twice holds the first
//! publish's envelope and label manifest, and nothing reaches them.

use crate::support;

use wheat_core::{ApplicationContext, Repository, Tree};
use wheat_store::{
    ContentStore, LocalPublicationStore, PackedObjectStore, RepositoryPublisher, SnapshotLimits,
    StoredInventory, StoredRecord, analyze_structure,
};

async fn pool() -> (
    tempfile::TempDir,
    PackedObjectStore,
    wheat_core::RepositoryEnvelopeId,
    wheat_core::RepositoryEnvelopeId,
) {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let first = publisher.initialize(&repo).await.unwrap();
    let id = content.put(b"hello").unwrap();
    let bytes = content.get(id).unwrap();
    repo.create_change(
        base,
        base,
        [],
        support::exact("a", None, &bytes),
        ApplicationContext::new(&content),
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
