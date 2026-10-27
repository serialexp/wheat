//! A repository whose changes are revised and joined in process, for the store's
//! integration and label-move tests.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use wheat_core::*;
use wheat_store::*;

pub struct CountingStore {
    pub inner: LooseObjectStore,
    pub reads: AtomicUsize,
}

#[async_trait]
impl ImmutableObjectStore for CountingStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

pub fn exact(path: &str, expected: Option<&[u8]>, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
            replacement: Some(FileValue {
                content: ContentId::of(replacement),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub objects: CountingStore,
    pub content: ContentStore,
    pub heads: LocalPublicationStore,
    pub base: ViewId,
    /// Change ids come from a counter, so the maps keyed by them have the same shape on
    /// every run. Like UUIDv7 ids they grow with creation order.
    pub changes: AtomicUsize,
}

impl Fixture {
    pub fn next_change(&self) -> ChangeId {
        let count = self.changes.fetch_add(1, Ordering::Relaxed) as u128 + 1;
        let tail = count.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835) >> 48;
        ChangeId::from_bytes(((count << 80) | tail).to_be_bytes())
    }

    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects = CountingStore {
            inner: LooseObjectStore::with_durability(
                dir.path().join("objects"),
                Durability::Deferred,
            ),
            reads: AtomicUsize::new(0),
        };
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap();
        Self {
            _dir: dir,
            objects,
            content,
            heads,
            base,
            changes: AtomicUsize::new(0),
        }
    }

    pub fn mutator(&self) -> PersistedRepositoryMutator<'_, CountingStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    pub fn reader(&self) -> RepositoryReader<'_, CountingStore> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    pub fn head(&self) -> RepositoryHead {
        self.heads.read().unwrap()
    }

    /// A new change writing `bytes` to `path` on top of `effective`, composed from
    /// `basis`. Returns the change and the view it makes.
    pub async fn create(
        &self,
        basis: ViewId,
        effective: ViewId,
        path: &str,
        bytes: &[u8],
    ) -> (ChangeId, ViewId) {
        self.create_committed_at(basis, effective, path, bytes, 0)
            .await
    }

    /// [`Self::create`], with the revision committed at `committed_at` seconds.
    pub async fn create_committed_at(
        &self,
        basis: ViewId,
        effective: ViewId,
        path: &str,
        bytes: &[u8],
        committed_at: i64,
    ) -> (ChangeId, ViewId) {
        self.content.put(bytes).unwrap();
        let change = self.next_change();
        let (prepared, outcome) = self
            .mutator()
            .create_on_frontier(
                self.head(),
                basis,
                effective,
                ChangeProposal {
                    id: change,
                    metadata: ChangeMetadata::default(),
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata {
                        committed_at: Timestamp {
                            seconds: committed_at,
                            offset_minutes: 0,
                        },
                        ..RevisionMetadata::default()
                    },
                    edits: exact(path, None, bytes),
                },
            )
            .await
            .unwrap();
        self.mutator().publish(prepared).await.unwrap();
        (change, outcome.resulting_view)
    }

    pub async fn integrate(
        &self,
        basis: ViewId,
        frontiers: &[ViewId],
    ) -> Result<IntegrationOutcome, RepositoryMutationError> {
        let (prepared, outcome) = self
            .mutator()
            .integrate(self.head(), basis, frontiers)
            .await?;
        self.mutator().publish(prepared).await.unwrap();
        Ok(outcome)
    }

    pub async fn frontiers(&self, basis: ViewId) -> Vec<ViewId> {
        let reader = self.reader();
        let state = reader
            .state(reader.envelope(self.head().envelope).await.unwrap().state)
            .await
            .unwrap();
        ViewLineageMapStore::new(&self.objects)
            .descendant_frontiers(state.view_lineage, basis, ViewLineageLimits::default())
            .await
            .unwrap()
    }

    /// Runs full validation on the published state.
    pub async fn validate(&self) {
        let reader = self.reader();
        let state = reader.envelope(self.head().envelope).await.unwrap().state;
        reader.validate_complete(state).await.unwrap();
    }

    pub async fn weight(&self, view: ViewId) -> u64 {
        self.reader().view(view).await.unwrap().order.weight
    }

    pub async fn file(&self, view: ViewId, path: &str) -> Option<ContentId> {
        let tree = self.reader().view(view).await.unwrap().tree;
        MerkleTreeStore::new(&self.objects)
            .lookup(tree, &[path.to_owned()])
            .await
            .unwrap()
            .map(|value| value.content)
    }
}

/// How a revision changes its predecessor's edits.
pub enum Revision<'a> {
    /// Replaces the predecessor's edits with one writing these bytes to `a`.
    Replace(&'a [u8]),
    /// Keeps the predecessor's edits and adds one writing `path`.
    Add(&'a str),
    /// Keeps the predecessor's edits except the one writing `path`.
    Remove(&'a str),
}

/// Revises `change` at its revision in `effective`, committed at `committed_at`, and
/// returns the view that makes.
pub async fn revise(
    fixture: &Fixture,
    basis: ViewId,
    effective: ViewId,
    change: ChangeId,
    revision: Revision<'_>,
    committed_at: i64,
) -> ViewId {
    let view = fixture.reader().view(effective).await.unwrap();
    let predecessor = RevisionMapStore::new(&fixture.objects)
        .get(view.revisions, change)
        .await
        .unwrap()
        .unwrap();
    let old = fixture.reader().revision(predecessor).await.unwrap();
    let edits = match revision {
        Revision::Replace(bytes) => {
            fixture.content.put(bytes).unwrap();
            let mut edits = exact("a", None, bytes);
            edits.transition.removed.extend(old.edits.iter().copied());
            edits
        }
        Revision::Add(path) => {
            fixture.content.put(path.as_bytes()).unwrap();
            let mut edits = exact(path, None, path.as_bytes());
            for id in &old.edits {
                edits
                    .edits
                    .insert(*id, fixture.reader().edit(*id).await.unwrap());
                edits.transition.preserved.insert(*id);
            }
            edits
        }
        Revision::Remove(path) => {
            let mut edits = RevisionEdits {
                edits: Default::default(),
                transition: EditTransition::default(),
            };
            for id in &old.edits {
                let edit = fixture.reader().edit(*id).await.unwrap();
                if edit.path == path {
                    edits.transition.removed.insert(*id);
                } else {
                    edits.edits.insert(*id, edit);
                    edits.transition.preserved.insert(*id);
                }
            }
            assert!(
                !edits.transition.removed.is_empty(),
                "no edit writes {path}"
            );
            edits
        }
    };
    let metadata = RevisionMetadata {
        committed_at: Timestamp {
            seconds: committed_at,
            offset_minutes: 0,
        },
        message: format!("committed at {committed_at}"),
        ..RevisionMetadata::default()
    };
    let (prepared, outcome) = fixture
        .mutator()
        .revise(
            fixture.head(),
            basis,
            effective,
            change,
            predecessor,
            metadata,
            edits,
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    outcome.resulting_view
}

pub async fn revision_in(fixture: &Fixture, view: ViewId, change: ChangeId) -> RevisionRecord {
    let view = fixture.reader().view(view).await.unwrap();
    let revision = RevisionMapStore::new(&fixture.objects)
        .get(view.revisions, change)
        .await
        .unwrap()
        .unwrap();
    fixture.reader().revision(revision).await.unwrap()
}

/// The files of `paths` that `view` holds.
pub async fn present(fixture: &Fixture, view: ViewId, paths: &[&str]) -> Vec<String> {
    let mut held = Vec::new();
    for path in paths {
        if fixture.file(view, path).await.is_some() {
            held.push((*path).to_owned());
        }
    }
    held
}
