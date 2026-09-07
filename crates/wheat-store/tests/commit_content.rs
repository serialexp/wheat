use async_trait::async_trait;
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wheat_core::*;
use wheat_store::*;
struct Counting {
    inner: ObjectStoreAdapter,
    trees: AtomicUsize,
    sets: AtomicUsize,
}
#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let o = self.inner.get(id).await?;
        if o.kind == ObjectKind::Tree {
            self.trees.fetch_add(1, Ordering::SeqCst);
        }
        Ok(o)
    }
    async fn put_if_absent(&self, id: ObjectId, b: &[u8]) -> Result<(), ObjectStoreError> {
        if validate_canonical_object(b)? == CanonicalObjectKind::ContentSetNode {
            self.sets.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put_if_absent(id, b).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}
fn proposal(n: u8, parent: Option<ChangeId>, edits: Vec<FileEdit>) -> ChangeProposal {
    ChangeProposal {
        id: ChangeId::from_bytes([n; 16]),
        metadata: Default::default(),
        revision_metadata: Default::default(),
        dependencies: parent.into_iter().collect(),
        transformation: Transformation { edits },
    }
}
#[tokio::test]
async fn noop_and_addition_do_not_walk_unchanged_subtrees_or_rebuild_content_set() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path());
    let objects = Counting {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "content-delta"),
        trees: AtomicUsize::new(0),
        sets: AtomicUsize::new(0),
    };
    let writer = PersistedCommitWriter::new(&objects, &content);
    let empty = writer.empty_view().await.unwrap();
    let edits = (0..64)
        .map(|n| FileEdit {
            path: format!("dir{n}/file"),
            expected: None,
            replacement: Some(content.put(format!("value{n}").as_bytes()).unwrap()),
        })
        .collect();
    let a = writer.write(empty, proposal(1, None, edits)).await.unwrap();
    objects.trees.store(0, Ordering::SeqCst);
    objects.sets.store(0, Ordering::SeqCst);
    let b = writer
        .write(a.resulting_view, proposal(2, Some(a.change_id), vec![]))
        .await
        .unwrap();
    assert_eq!(objects.trees.load(Ordering::SeqCst), 0);
    assert_eq!(objects.sets.load(Ordering::SeqCst), 0);
    let id = content.put(b"new").unwrap();
    writer
        .write(
            b.resulting_view,
            proposal(
                3,
                Some(b.change_id),
                vec![FileEdit {
                    path: "new".into(),
                    expected: None,
                    replacement: Some(id),
                }],
            ),
        )
        .await
        .unwrap();
    assert!(
        objects.trees.load(Ordering::SeqCst) < 8,
        "addition must not traverse 64 untouched directories"
    );
    assert!(
        objects.sets.load(Ordering::SeqCst) < 34,
        "only one content key's path can be rewritten"
    );
}
