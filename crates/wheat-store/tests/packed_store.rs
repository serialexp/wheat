use wheat_core::{ContentId, DirectoryNode, ObjectId};
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
        2
    );
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
