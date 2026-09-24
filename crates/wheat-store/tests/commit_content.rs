use async_trait::async_trait;
use object_store::memory::InMemory;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
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
        let object = self.inner.get(id).await?;
        if object.kind == ObjectKind::Tree {
            self.trees.fetch_add(1, Ordering::SeqCst);
        }
        Ok(object)
    }
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        if matches!(
            validate_canonical_object(bytes)?,
            CanonicalObjectKind::ContentSetSnapshot | CanonicalObjectKind::ContentSetDelta
        ) {
            self.sets.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put_if_absent(id, bytes).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}
fn proposal(n: u8, parent: Option<ChangeId>, records: Vec<EditRecord>) -> ChangeProposal {
    let edits = records
        .into_iter()
        .map(|record| (record.id(), record))
        .collect::<BTreeMap<_, _>>();
    ChangeProposal {
        id: ChangeId::from_bytes([n; 16]),
        metadata: Default::default(),
        revision_metadata: Default::default(),
        dependencies: parent.into_iter().collect(),
        edits: RevisionEdits {
            transition: EditTransition {
                added: edits.keys().copied().collect(),
                ..Default::default()
            },
            edits,
        },
    }
}
fn exact(path: String, replacement: Option<FileValue>) -> EditRecord {
    exact_change(path, None, replacement)
}

fn exact_change(
    path: String,
    expected: Option<FileValue>,
    replacement: Option<FileValue>,
) -> EditRecord {
    EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected,
            replacement,
        },
    )
    .unwrap()
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
        .map(|n| {
            exact(
                format!("dir{n}/file"),
                Some(FileValue {
                    content: content.put(format!("value{n}").as_bytes()).unwrap(),
                    kind: FileKind::Regular,
                }),
            )
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
                vec![exact(
                    "new".into(),
                    Some(FileValue {
                        content: id,
                        kind: FileKind::Regular,
                    }),
                )],
            ),
        )
        .await
        .unwrap();
    assert!(objects.trees.load(Ordering::SeqCst) < 8);
    assert!(
        objects.sets.load(Ordering::SeqCst) < 40,
        "content-set rebuilding must remain bounded independently of key width"
    );
}

#[tokio::test]
async fn many_deletions_scan_the_resulting_tree_once() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path());
    let objects = Counting {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "content-removals"),
        trees: AtomicUsize::new(0),
        sets: AtomicUsize::new(0),
    };
    let writer = PersistedCommitWriter::new(&objects, &content);
    let empty = writer.empty_view().await.unwrap();
    let mut values = Vec::new();
    let seed_edits = (0..64)
        .map(|n| {
            let value = FileValue {
                content: content.put(format!("value{n}").as_bytes()).unwrap(),
                kind: FileKind::Regular,
            };
            values.push(value);
            exact(format!("dir{n}/file"), Some(value))
        })
        .collect();
    let seed = writer
        .write(empty, proposal(1, None, seed_edits))
        .await
        .unwrap();
    objects.trees.store(0, Ordering::SeqCst);
    let deletions = values
        .into_iter()
        .enumerate()
        .map(|(n, value)| exact_change(format!("dir{n}/file"), Some(value), None))
        .collect();
    writer
        .write(
            seed.resulting_view,
            proposal(2, Some(seed.change_id), deletions),
        )
        .await
        .unwrap();
    assert!(
        objects.trees.load(Ordering::SeqCst) < 512,
        "removed content candidates must share one resulting-tree traversal"
    );
}

#[tokio::test]
async fn deleting_one_of_two_shared_visible_blobs_keeps_the_manifest_entry() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let writer = PersistedCommitWriter::new(&objects, &content);
    let empty = writer.empty_view().await.unwrap();
    let shared = content.put(b"shared").unwrap();
    let value = FileValue {
        content: shared,
        kind: FileKind::Regular,
    };
    let seed = writer
        .write(
            empty,
            proposal(
                1,
                None,
                vec![
                    exact("a".into(), Some(value)),
                    exact("b".into(), Some(value)),
                ],
            ),
        )
        .await
        .unwrap();
    let deletion = EditRecord::new(
        "a",
        EditOperation::ExactFile {
            expected: Some(value),
            replacement: None,
        },
    )
    .unwrap();
    let result = writer
        .write(
            seed.resulting_view,
            proposal(2, Some(seed.change_id), vec![deletion]),
        )
        .await
        .unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let view = reader.view(result.resulting_view).await.unwrap();
    let manifest = reader.manifest(view.required_content).await.unwrap();
    assert!(
        ContentSetStore::new(&objects)
            .contains(manifest.content, shared)
            .await
            .unwrap(),
        "the unchanged second path still visibly references the shared blob"
    );
}
