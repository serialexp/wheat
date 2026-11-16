//! Restoring a served repository to a snapshot: the repository's head becomes the
//! snapshot's envelope, its payload checked against the manifest, with the content
//! it relies on named, replacing whatever was published since.

use std::sync::Arc;

use wheat_server::restore::{RestoreFailure, restore_from};
use wheat_server::write::{
    ExpectedLabel::{Absent, View},
    WriteFailure, write,
};
use wheat_store::{
    NativeSnapshot, RepositoryReader, SnapshotError, SnapshotId, SnapshotLimits, export_snapshot,
};

use crate::support::served::{
    Named, change, commit, file, head, id, labels, named_repository, principal, server_write,
};

/// A snapshot of `repo`'s head.
async fn snapshot(repo: &Named) -> NativeSnapshot {
    let content = repo.repo.objects.content_store();
    export_snapshot(
        &repo.repo.objects,
        &content,
        &repo.repo.heads(),
        SnapshotLimits::default(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_restore_returns_the_head_to_the_snapshot() {
    let repo = named_repository().await;
    let one = commit(&repo.entry, 1, repo.repo.base, Absent, "one.txt", b"one").await;
    let taken = snapshot(&repo).await;
    // Published after the snapshot: a change, and a label.
    let two = commit(&repo.entry, 2, one, View(one), "two.txt", b"two").await;
    write(
        &repo.entry,
        server_write(3, vec![change("local", Absent, two)]),
    )
    .await
    .unwrap();
    let before = head(&repo.entry).await;

    let restored = restore_from(
        &repo.entry,
        &principal(),
        Arc::new(taken.clone()),
        SnapshotLimits::default(),
    )
    .await
    .unwrap();

    assert!(restored.moved);
    assert_eq!(restored.head, head(&repo.entry).await);
    assert_eq!(restored.head.envelope, taken.manifest.envelope);
    assert_eq!(
        restored.head.generation,
        before.generation + 1,
        "the generation moves on"
    );
    assert_eq!(
        labels(&repo.repo, restored.head).await,
        vec![("main".to_string(), one)],
        "main moved back and the later label is gone"
    );
}

#[tokio::test]
async fn a_restore_into_another_repository_stores_and_names_the_snapshot() {
    let source = named_repository().await;
    let one = commit(
        &source.entry,
        1,
        source.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let taken = snapshot(&source).await;
    let target = named_repository().await;

    let restored = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken.clone()),
        SnapshotLimits::default(),
    )
    .await
    .unwrap();

    assert!(restored.moved);
    assert_eq!(restored.head.envelope, taken.manifest.envelope);
    let content = target.repo.objects.content_store();
    RepositoryReader::new(&target.repo.objects, &content)
        .validate_envelope(restored.head.envelope)
        .await
        .unwrap();
    assert_eq!(
        file(&target.repo, one, "one.txt").await,
        Some(b"one".to_vec())
    );
    let named = target.naming.calls.lock().unwrap().concat();
    assert!(named.contains(&id(b"one")), "{named:?}");

    let again = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken),
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    assert!(!again.moved, "the head is at the snapshot already");
    assert_eq!(again.head, restored.head);
}

#[tokio::test]
async fn a_restore_stores_a_blob_over_the_threshold_as_its_chunks() {
    let source = named_repository().await;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut large = Vec::new();
    while large.len() < wheat_core::CHUNKED_CONTENT_THRESHOLD as usize + (3 << 20) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        large.extend_from_slice(&state.to_le_bytes());
    }
    // `commit` takes a file's bytes for the life of the test.
    let large: &'static [u8] = large.leak();
    let one = commit(
        &source.entry,
        1,
        source.repo.base,
        Absent,
        "large.bin",
        large,
    )
    .await;
    let taken = snapshot(&source).await;
    assert_eq!(taken.manifest.chunked.len(), 1);
    let target = named_repository().await;

    let restored = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken),
        SnapshotLimits::default(),
    )
    .await
    .unwrap();

    assert!(restored.moved);
    let content = target.repo.objects.content_store();
    assert!(content.chunk_manifest(id(large)).unwrap().is_some());
    content.verify(id(large)).unwrap();
    assert_eq!(
        file(&target.repo, one, "large.bin").await.as_deref(),
        Some(large)
    );
    let named = target.naming.calls.lock().unwrap().concat();
    assert!(named.contains(&id(large)), "the blob is named");
}

#[tokio::test]
async fn a_snapshot_that_is_not_the_one_claimed_leaves_the_head() {
    let source = named_repository().await;
    commit(
        &source.entry,
        1,
        source.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mut taken = snapshot(&source).await;
    taken.manifest_id = SnapshotId([7; 32]);
    let target = named_repository().await;
    let before = head(&target.entry).await;

    let failure = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken),
        SnapshotLimits::default(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            failure,
            RestoreFailure::Snapshot(SnapshotError::InvalidManifest(_))
        ),
        "{failure:?}"
    );
    assert_eq!(head(&target.entry).await, before);
}

#[tokio::test]
async fn a_snapshot_missing_its_content_leaves_the_head() {
    let source = named_repository().await;
    commit(
        &source.entry,
        1,
        source.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mut taken = snapshot(&source).await;
    taken.contents.clear();
    let target = named_repository().await;
    let before = head(&target.entry).await;

    let failure = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken),
        SnapshotLimits::default(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            failure,
            RestoreFailure::Snapshot(SnapshotError::InventoryMismatch)
        ),
        "{failure:?}"
    );
    assert_eq!(head(&target.entry).await, before);
}

#[tokio::test]
async fn a_quiesced_network_refuses_a_restore() {
    let source = named_repository().await;
    let taken = snapshot(&source).await;
    let target = named_repository().await;
    let held = wheat_server::quiesce::Quiesced::hold(Arc::clone(&target.entry)).await;

    let failure = restore_from(
        &target.entry,
        &principal(),
        Arc::new(taken),
        SnapshotLimits::default(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            failure,
            RestoreFailure::Write(WriteFailure::Quiesced { .. })
        ),
        "{failure:?}"
    );
    drop(held);
}
