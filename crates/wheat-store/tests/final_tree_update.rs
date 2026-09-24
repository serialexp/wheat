use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use wheat_core::{ContentId, FileKind, FileValue, ObjectId};
use wheat_store::{
    ImmutableObjectStore, LooseObjectStore, MerkleTreeError, MerkleTreeStore, ObjectStoreError,
    StoredObject, TreeEdit,
};

fn file(label: &str) -> FileValue {
    FileValue {
        content: ContentId::of(label.as_bytes()),
        kind: FileKind::Regular,
    }
}

fn edit(path: &str, value: Option<&str>) -> TreeEdit {
    TreeEdit {
        path: path.split('/').map(str::to_owned).collect(),
        value: value.map(file),
    }
}

struct ObservedStore<S> {
    inner: S,
    reads: AtomicUsize,
    writes: Mutex<Vec<ObjectId>>,
}

impl<S> ObservedStore<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
            writes: Mutex::new(Vec::new()),
        }
    }

    fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.writes.lock().unwrap().clear();
    }
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for ObservedStore<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.writes.lock().unwrap().push(id);
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

async fn old_two_pass<S: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, S>,
    root: wheat_core::TreeId,
    edits: &[TreeEdit],
) -> wheat_core::TreeId {
    let deletions = edits
        .iter()
        .filter(|edit| edit.value.is_none())
        .cloned()
        .collect::<Vec<_>>();
    let insertions = edits
        .iter()
        .filter(|edit| edit.value.is_some())
        .cloned()
        .collect::<Vec<_>>();
    let deleted = trees.apply_edits(root, &deletions).await.unwrap();
    trees.apply_edits(deleted, &insertions).await.unwrap()
}

#[tokio::test]
async fn final_update_matches_two_pass_for_nested_wide_and_deep_edits() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let trees = MerkleTreeStore::new(&objects);
    let mut seed = Vec::new();
    for index in 0..128 {
        seed.push(edit(&format!("wide/{index:03}"), Some("old")));
    }
    seed.push(edit("deep/a/b/c/d/e/leaf", Some("old-deep")));
    seed.push(edit("untouched/leaf", Some("shared")));
    let root = trees
        .apply_edits(trees.empty_root().await.unwrap(), &seed)
        .await
        .unwrap();
    let edits = vec![
        edit("wide/003", None),
        edit("wide/003/child", Some("shape")),
        edit("wide/064", Some("changed")),
        edit("wide/127", None),
        edit("deep/a/b/c/d/e/leaf", Some("new-deep")),
        edit("new/deep/a/b/c/d/e/leaf", Some("new")),
    ];

    let expected = old_two_pass(&trees, root, &edits).await;
    let actual = trees.apply_final_edits(root, &edits).await.unwrap();
    assert_eq!(actual, expected, "final canonical tree ID changed");
}

#[tokio::test]
async fn mixed_update_does_not_persist_deletion_only_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let objects = Arc::new(ObservedStore::new(LooseObjectStore::new(dir.path())));
    let trees = MerkleTreeStore::new(objects.as_ref());
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit("a/old", Some("old")),
                edit("a/keep", Some("keep")),
                edit("elsewhere", Some("same")),
            ],
        )
        .await
        .unwrap();
    let edits = [edit("a/old", Some("new"))];

    objects.reset();
    let deletion_root = trees
        .apply_edits(root, &[edit("a/old", None)])
        .await
        .unwrap();
    objects.reset();
    let final_root = trees.apply_final_edits(root, &edits).await.unwrap();
    let writes = objects.writes.lock().unwrap().clone();

    assert_ne!(deletion_root, final_root);
    assert!(!writes.contains(&ObjectId::from(deletion_root)));
    assert!(writes.contains(&ObjectId::from(final_root)));
    assert_eq!(writes.len(), 2, "only the final child and root are written");
}

#[tokio::test]
async fn rejected_transition_persists_no_partial_final_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let objects = ObservedStore::new(LooseObjectStore::new(dir.path()));
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit("a/old", Some("old")),
                edit("blocked/child", Some("blocker")),
            ],
        )
        .await
        .unwrap();

    objects.reset();
    assert!(matches!(
        trees
            .apply_final_edits(
                root,
                &[
                    edit("a/old", Some("new")),
                    edit("blocked", Some("replacement")),
                ],
            )
            .await,
        Err(MerkleTreeError::FileInPath(_))
    ));
    assert!(objects.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shape_transitions_require_explicit_exhaustive_removals() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let trees = MerkleTreeStore::new(&objects);
    let empty = trees.empty_root().await.unwrap();
    let file_root = trees
        .apply_edits(empty, &[edit("node", Some("file"))])
        .await
        .unwrap();

    assert!(matches!(
        trees
            .apply_final_edits(file_root, &[edit("node/child", Some("child"))])
            .await,
        Err(MerkleTreeError::FileInPath(_))
    ));
    assert!(matches!(
        trees
            .apply_final_edits(
                file_root,
                &[edit("missing", None), edit("node/child", Some("child"))],
            )
            .await,
        Err(MerkleTreeError::FileInPath(_))
    ));
    let directory_root = trees
        .apply_final_edits(
            file_root,
            &[edit("node", None), edit("node/child", Some("child"))],
        )
        .await
        .unwrap();
    let with_blocker = trees
        .apply_edits(directory_root, &[edit("node/blocker", Some("blocker"))])
        .await
        .unwrap();
    assert!(matches!(
        trees
            .apply_final_edits(
                with_blocker,
                &[edit("node/child", None), edit("node", Some("file-2"))],
            )
            .await,
        Err(MerkleTreeError::FileInPath(_))
    ));
    let file_again = trees
        .apply_final_edits(
            with_blocker,
            &[
                edit("node/child", None),
                edit("node/blocker", None),
                edit("node", Some("file-2")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        trees.lookup(file_again, &["node".into()]).await.unwrap(),
        Some(file("file-2"))
    );
}

#[tokio::test]
async fn representative_wide_update_has_linear_io_budget() {
    const FILES: usize = 2_000;
    const CHANGES: usize = 500;
    let dir = tempfile::tempdir().unwrap();
    let objects = ObservedStore::new(LooseObjectStore::new(dir.path()));
    let trees = MerkleTreeStore::new(&objects);
    let seed = (0..FILES)
        .map(|index| edit(&format!("wide/{index:04}"), Some("old")))
        .collect::<Vec<_>>();
    let root = trees
        .apply_edits(trees.empty_root().await.unwrap(), &seed)
        .await
        .unwrap();
    let edits = (0..CHANGES)
        .map(|index| edit(&format!("wide/{index:04}"), Some("new")))
        .collect::<Vec<_>>();

    objects.reset();
    trees.apply_final_edits(root, &edits).await.unwrap();
    assert!(objects.reads.load(Ordering::Relaxed) <= 2);
    assert!(objects.writes.lock().unwrap().len() <= 2);
}
