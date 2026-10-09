//! Joining frontiers that grew apart below one basis into one view.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use wheat_core::*;
use wheat_store::*;

struct CountingStore {
    inner: LooseObjectStore,
    reads: AtomicUsize,
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

fn exact(path: &str, expected: Option<&[u8]>, replacement: &[u8]) -> RevisionEdits {
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

struct Fixture {
    _dir: tempfile::TempDir,
    objects: CountingStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    base: ViewId,
}

impl Fixture {
    async fn new() -> Self {
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
        }
    }

    fn mutator(&self) -> PersistedRepositoryMutator<'_, CountingStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    fn reader(&self) -> RepositoryReader<'_, CountingStore> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    fn head(&self) -> RepositoryHead {
        self.heads.read().unwrap()
    }

    /// A new change writing `bytes` to `path` on top of `effective`, composed from
    /// `basis`. Returns the change and the view it makes.
    async fn create(
        &self,
        basis: ViewId,
        effective: ViewId,
        path: &str,
        bytes: &[u8],
    ) -> (ChangeId, ViewId) {
        self.content.put(bytes).unwrap();
        let change = ChangeId::new();
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
                    revision_metadata: RevisionMetadata::default(),
                    edits: exact(path, None, bytes),
                },
            )
            .await
            .unwrap();
        self.mutator().publish(prepared).await.unwrap();
        (change, outcome.resulting_view)
    }

    async fn integrate(
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

    async fn frontiers(&self, basis: ViewId) -> Vec<ViewId> {
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

    async fn file(&self, view: ViewId, path: &str) -> Option<ContentId> {
        let tree = self.reader().view(view).await.unwrap().tree;
        MerkleTreeStore::new(&self.objects)
            .lookup(tree, &[path.to_owned()])
            .await
            .unwrap()
            .map(|value| value.content)
    }
}

#[tokio::test]
async fn two_frontiers_join_into_one_view_holding_both() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, left) = fixture.create(base, base, "a", b"a").await;
    let (b, right) = fixture.create(base, base, "b", b"b").await;
    assert_eq!(fixture.frontiers(base).await.len(), 2);

    let outcome = fixture.integrate(base, &[left, right]).await.unwrap();

    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(outcome.composed, expected);
    assert!(outcome.conflicted.is_empty());
    let joined = outcome.resulting_view;
    assert_eq!(fixture.file(joined, "a").await, Some(ContentId::of(b"a")));
    assert_eq!(fixture.file(joined, "b").await, Some(ContentId::of(b"b")));
    assert_eq!(fixture.frontiers(base).await, [joined]);
    let reader = fixture.reader();
    reader
        .validate_complete(
            reader
                .envelope(fixture.head().envelope)
                .await
                .unwrap()
                .state,
        )
        .await
        .unwrap();

    // Work goes on from the joined view.
    let (_, next) = fixture.create(base, joined, "c", b"c").await;
    assert_eq!(fixture.frontiers(base).await, [next]);
}

#[tokio::test]
async fn one_frontier_joins_into_itself() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (_, first) = fixture.create(base, base, "a", b"a").await;
    let (_, second) = fixture.create(base, first, "b", b"b").await;

    let outcome = fixture.integrate(base, &[second]).await.unwrap();

    assert_eq!(outcome.resulting_view, second);
    assert_eq!(fixture.frontiers(base).await, [second]);
}

#[tokio::test]
async fn two_sides_writing_one_path_join_with_a_durable_conflict() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (x, left) = fixture.create(base, base, "same", b"left").await;
    let (y, right) = fixture.create(base, base, "same", b"right").await;

    let outcome = fixture.integrate(base, &[left, right]).await.unwrap();

    // The later change in canonical order finds the path already written.
    assert_eq!(outcome.conflicted, [x.max(y)]);
    let view = fixture.reader().view(outcome.resulting_view).await.unwrap();
    let conflicts = ConflictSetStore::new(&fixture.objects);
    let mut cursor = conflicts.cursor(view.unresolved_conflicts).await.unwrap();
    assert!(cursor.next().await.unwrap().is_some());
    assert_eq!(fixture.frontiers(base).await, [outcome.resulting_view]);
}

/// Revises `change`, whose current revision is `predecessor`, in the view `effective`.
async fn revise(
    fixture: &Fixture,
    basis: ViewId,
    effective: ViewId,
    change: ChangeId,
    bytes: &[u8],
) -> ViewId {
    fixture.content.put(bytes).unwrap();
    let view = fixture.reader().view(effective).await.unwrap();
    let predecessor = RevisionMapStore::new(&fixture.objects)
        .get(view.revisions, change)
        .await
        .unwrap()
        .unwrap();
    let old = fixture.reader().revision(predecessor).await.unwrap();
    let old_edit = *old.edits.iter().next().unwrap();
    let mut edits = exact("a", None, bytes);
    edits.transition.removed.insert(old_edit);
    let (prepared, outcome) = fixture
        .mutator()
        .revise(
            fixture.head(),
            basis,
            effective,
            change,
            predecessor,
            RevisionMetadata::default(),
            edits,
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    outcome.resulting_view
}

#[tokio::test]
async fn frontiers_with_different_revisions_of_a_change_are_refused() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let (_, beside) = fixture.create(base, first, "b", b"b").await;
    let revised = revise(&fixture, base, first, a, b"a2").await;

    let error = fixture
        .integrate(base, &[beside, revised])
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RepositoryMutationError::DivergentRevisions(changes) if changes == &[a]),
        "{error}"
    );
}

#[tokio::test]
async fn a_frontier_that_revises_its_basis_is_refused() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let revised = revise(&fixture, base, first, a, b"a2").await;

    let error = fixture.integrate(first, &[revised]).await.unwrap_err();

    assert!(
        matches!(&error, RepositoryMutationError::FrontierRevisesBasis(changes) if changes == &[a]),
        "{error}"
    );
}

/// Joining one change on each side reads what the frontiers add, so a longer history
/// below the basis adds only the few radix levels it deepens.
#[tokio::test]
async fn integration_reads_what_the_frontiers_add() {
    let mut reads = Vec::new();
    for history in [8, 80] {
        let fixture = Fixture::new().await;
        let mut basis = fixture.base;
        for index in 0..history {
            basis = fixture
                .create(fixture.base, basis, &format!("history/{index}"), b"h")
                .await
                .1;
        }
        let (_, left) = fixture.create(fixture.base, basis, "left", b"left").await;
        let (_, right) = fixture.create(fixture.base, basis, "right", b"right").await;
        fixture.objects.reads.store(0, Ordering::Relaxed);
        let (_, outcome) = fixture
            .mutator()
            .integrate(fixture.head(), basis, &[left, right])
            .await
            .unwrap();
        assert_eq!(outcome.composed.len(), 2);
        reads.push(fixture.objects.reads.load(Ordering::Relaxed));
    }
    assert!(
        reads[1] <= reads[0] + 16,
        "reads grew with the history: {reads:?}"
    );
}
