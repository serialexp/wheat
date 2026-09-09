use wheat_core::{ContentId, DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId};
use wheat_store::{ImmutableObjectStore, PackedObjectStore};

#[tokio::test]
async fn metadata_and_content_share_segments_without_namespace_confusion() {
    let root = tempfile::tempdir().unwrap();
    let bytes = DirectoryNode::default().canonical_bytes();
    let id = ObjectId::of_canonical_bytes(&bytes);
    let content_id = ContentId::of(&bytes);
    {
        let objects = PackedObjectStore::open(root.path()).unwrap();
        let content = objects.content_store();
        objects.put_batch(&[(id, bytes.clone())], 1).await.unwrap();
        content.put(&bytes).unwrap();
        assert_eq!(objects.get(id).await.unwrap().canonical_bytes, bytes);
        assert_eq!(content.get(content_id).unwrap(), bytes);
    }
    let objects = PackedObjectStore::open(root.path()).unwrap();
    assert_eq!(objects.get(id).await.unwrap().canonical_bytes, bytes);
    assert_eq!(objects.content_store().get(content_id).unwrap(), bytes);
    assert!(!root.path().join("loose-content").exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("packed/segments"))
            .unwrap()
            .count(),
        0
    );
    assert!(root.path().join("packed/active.segment").is_file());
}

#[test]
fn close_requires_exclusive_ownership_and_releases_the_catalog_lock() {
    let root = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open_deferred(root.path()).unwrap();
    let content = objects.content_store();
    let error = objects.close().unwrap_err();
    assert!(error.to_string().contains("StillReferenced"));
    drop(content);

    let objects = PackedObjectStore::open_deferred(root.path()).unwrap();
    objects.close().unwrap();
    PackedObjectStore::open(root.path())
        .unwrap()
        .close()
        .unwrap();
}

#[tokio::test]
async fn deferred_batches_share_one_pack_and_reopen_after_consuming_close() {
    let root = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open_deferred(root.path()).unwrap();
    let mut expected = Vec::new();
    for index in 0_u64..128 {
        let bytes = DirectoryNode::new([(
            format!("file-{index}"),
            DirectoryEntry::File(FileValue {
                content: ContentId::of(&index.to_be_bytes()),
                kind: FileKind::Regular,
            }),
        )])
        .unwrap()
        .canonical_bytes();
        let id = ObjectId::of_canonical_bytes(&bytes);
        objects.put_batch(&[(id, bytes.clone())], 1).await.unwrap();
        assert_eq!(objects.get(id).await.unwrap().canonical_bytes, bytes);
        expected.push((id, bytes));
    }
    objects.close().unwrap();

    assert_eq!(
        std::fs::read_dir(root.path().join("packed/segments"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("packed/catalog"))
            .unwrap()
            .count(),
        1
    );
    let reopened = PackedObjectStore::open(root.path()).unwrap();
    for (id, bytes) in expected {
        assert_eq!(reopened.get(id).await.unwrap().canonical_bytes, bytes);
    }
}

#[test]
fn large_content_uses_standalone_fallback() {
    let root = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open_with_loose_threshold(root.path(), 16).unwrap();
    let content = objects.content_store();
    let id = content.put(&[9; 17]).unwrap();
    assert!(content.path(id).is_file());
    assert_eq!(content.get(id).unwrap(), [9; 17]);
}

#[tokio::test]
async fn large_metadata_uses_standalone_fallback() {
    let root = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open_with_loose_threshold(root.path(), 1).unwrap();
    let bytes = DirectoryNode::default().canonical_bytes();
    let id = ObjectId::of_canonical_bytes(&bytes);

    objects.put_if_absent(id, &bytes).await.unwrap();

    assert_eq!(objects.get(id).await.unwrap().canonical_bytes, bytes);
}
