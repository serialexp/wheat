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
    /// Change ids come from a counter, so the maps keyed by them have the same shape on
    /// every run. Like UUIDv7 ids they grow with creation order.
    changes: AtomicUsize,
}

impl Fixture {
    fn next_change(&self) -> ChangeId {
        let count = self.changes.fetch_add(1, Ordering::Relaxed) as u128 + 1;
        let tail = count.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835) >> 48;
        ChangeId::from_bytes(((count << 80) | tail).to_be_bytes())
    }

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
            changes: AtomicUsize::new(0),
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

    /// Runs full validation on the published state.
    async fn validate(&self) {
        let reader = self.reader();
        let state = reader.envelope(self.head().envelope).await.unwrap().state;
        reader.validate_complete(state).await.unwrap();
    }

    async fn weight(&self, view: ViewId) -> u64 {
        self.reader().view(view).await.unwrap().order.weight
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

/// How a revision changes its predecessor's edits.
enum Revision<'a> {
    /// Replaces the predecessor's edits with one writing these bytes to `a`.
    Replace(&'a [u8]),
    /// Keeps the predecessor's edits and adds one writing `path`.
    Add(&'a str),
}

/// Revises `change` at its revision in `effective`, committed at `committed_at`, and
/// returns the view that makes.
async fn revise(
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

async fn revision_in(fixture: &Fixture, view: ViewId, change: ChangeId) -> RevisionRecord {
    let view = fixture.reader().view(view).await.unwrap();
    let revision = RevisionMapStore::new(&fixture.objects)
        .get(view.revisions, change)
        .await
        .unwrap()
        .unwrap();
    fixture.reader().revision(revision).await.unwrap()
}

#[tokio::test]
async fn a_revision_another_one_follows_is_superseded() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let (_, beside) = fixture.create(base, first, "b", b"b").await;
    let revised = revise(&fixture, base, first, a, Revision::Replace(b"a2"), 1).await;

    let outcome = fixture.integrate(base, &[beside, revised]).await.unwrap();

    assert!(outcome.combined.is_empty());
    assert!(outcome.conflicted.is_empty());
    let joined = outcome.resulting_view;
    assert_eq!(fixture.file(joined, "a").await, Some(ContentId::of(b"a2")));
    assert_eq!(fixture.file(joined, "b").await, Some(ContentId::of(b"b")));
}

/// Two revisions that each add to the same predecessor become one revision with both
/// additions, following the earlier one and described by the later one.
#[tokio::test]
async fn revisions_that_went_their_own_ways_combine_by_date() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let later = revise(&fixture, base, first, a, Revision::Add("x"), 200).await;
    let earlier = revise(&fixture, base, first, a, Revision::Add("y"), 100).await;
    let earlier_revision = RevisionMapStore::new(&fixture.objects)
        .get(fixture.reader().view(earlier).await.unwrap().revisions, a)
        .await
        .unwrap()
        .unwrap();

    let outcome = fixture.integrate(base, &[later, earlier]).await.unwrap();

    assert_eq!(outcome.combined.keys().collect::<Vec<_>>(), [&a]);
    assert!(outcome.conflicted.is_empty());
    let joined = outcome.resulting_view;
    for (path, bytes) in [("a", b"a1".as_slice()), ("x", b"x"), ("y", b"y")] {
        assert_eq!(fixture.file(joined, path).await, Some(ContentId::of(bytes)));
    }
    let combined = revision_in(&fixture, joined, a).await;
    let later_revision = revision_in(&fixture, later, a).await.id();
    assert_eq!(combined.predecessor, Some(earlier_revision));
    assert_eq!(combined.combines, [later_revision].into());
    // The original revision is generation 0, each side's 1, the combination 2.
    assert_eq!(combined.generation, 2);
    assert_eq!(combined.metadata.message, "committed at 200");
    assert_eq!(combined.edits.len(), 3);
    assert_eq!(combined.transition.added.len(), 1);
    assert_eq!(combined.transition.preserved.len(), 2);
    for frontier in [later, earlier] {
        assert!(fixture.weight(joined).await > fixture.weight(frontier).await);
    }
    fixture.validate().await;
}

/// Squashing a combined revision writes an ordinary one of a lower generation. The
/// frontiers that held the inputs stay retained beside it, and every edge still
/// increases the weight.
#[tokio::test]
async fn a_combined_revision_squashes_into_a_valid_history() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let later = revise(&fixture, base, first, a, Revision::Add("x"), 200).await;
    let earlier = revise(&fixture, base, first, a, Revision::Add("y"), 100).await;
    let joined = fixture
        .integrate(base, &[later, earlier])
        .await
        .unwrap()
        .resulting_view;
    let combined = revision_in(&fixture, joined, a).await.id();

    let request = SquashRewriteRequest {
        expected_revisions: [(a, combined)].into(),
        retained_predecessors: [(a, None)].into(),
        revision_metadata: [(
            a,
            RevisionMetadata {
                message: "squashed".into(),
                ..Default::default()
            },
        )]
        .into(),
        authority_roots: [base, later, earlier, joined].into(),
        authority_view_sets: Default::default(),
        authority_revision_sets: Default::default(),
    };
    let prepared = RepositoryRewriter::new(&fixture.objects, &fixture.content, &fixture.heads)
        .prepare_squash(fixture.head(), base, joined, request)
        .await
        .unwrap()
        .unwrap();
    let squashed = prepared.mapping().revisions[&combined];
    let resulting = prepared.resulting_view();
    finalize_repository_rewrite(&fixture.heads, prepared).unwrap();

    let squashed = fixture.reader().revision(squashed).await.unwrap();
    assert!(squashed.combines.is_empty());
    assert_eq!(squashed.generation, 0);
    assert_eq!(fixture.weight(resulting).await, 1);
    fixture.validate().await;
}

/// Two revisions with the same edits still get a combined revision, which descends
/// from both: reusing either would give the other frontier an edge into a view no
/// heavier than itself.
#[tokio::test]
async fn revisions_with_the_same_edits_still_combine() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let left = revise(&fixture, base, first, a, Revision::Add("x"), 100).await;
    let right = revise(&fixture, base, first, a, Revision::Add("x"), 200).await;
    let (left_revision, right_revision) = (
        revision_in(&fixture, left, a).await,
        revision_in(&fixture, right, a).await,
    );
    assert_eq!(left_revision.edits, right_revision.edits);

    let outcome = fixture.integrate(base, &[left, right]).await.unwrap();

    assert_eq!(outcome.combined.keys().collect::<Vec<_>>(), [&a]);
    let joined = outcome.resulting_view;
    let combined = revision_in(&fixture, joined, a).await;
    assert_eq!(combined.predecessor, Some(left_revision.id()));
    assert_eq!(combined.combines, [right_revision.id()].into());
    assert_eq!(combined.edits, left_revision.edits);
    for frontier in [left, right] {
        assert!(fixture.weight(joined).await > fixture.weight(frontier).await);
    }
    fixture.validate().await;
}

/// A combined revision may find one of its edits already made where it lands, here
/// by a change that sorts before it, and still validates.
#[tokio::test]
async fn a_combined_revision_may_find_an_edit_already_made() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    // `q` sorts first (lower id, no dependencies) and writes `x`.
    let (_, written) = fixture.create(base, base, "x", b"x").await;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    // One side's revision of `a` also writes `x`; the other's writes `y`.
    let with_x = revise(&fixture, base, first, a, Revision::Add("x"), 100).await;
    let with_y = revise(&fixture, base, first, a, Revision::Add("y"), 200).await;

    let outcome = fixture
        .integrate(base, &[written, with_x, with_y])
        .await
        .unwrap();

    assert_eq!(outcome.combined.keys().collect::<Vec<_>>(), [&a]);
    assert!(outcome.conflicted.is_empty());
    let joined = outcome.resulting_view;
    for (path, bytes) in [("a", b"a1".as_slice()), ("x", b"x"), ("y", b"y")] {
        assert_eq!(fixture.file(joined, path).await, Some(ContentId::of(bytes)));
    }
    fixture.validate().await;
}

/// Up to 65 revisions of one change that went their own ways combine into one; more
/// are refused, naming the change.
#[tokio::test]
async fn at_most_sixty_five_revisions_combine_at_once() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let mut frontiers = Vec::new();
    for index in 0..=MAX_COMBINES + 1 {
        let path = format!("side/{index:02}");
        let at = i64::try_from(index).unwrap();
        frontiers.push(revise(&fixture, base, first, a, Revision::Add(&path), at).await);
    }

    let error = fixture.integrate(base, &frontiers).await.unwrap_err();
    assert!(
        matches!(&error, RepositoryMutationError::TooManyRevisions { change, count: 66 } if *change == a),
        "{error}"
    );

    let outcome = fixture
        .integrate(base, &frontiers[..=MAX_COMBINES])
        .await
        .unwrap();
    let combined = revision_in(&fixture, outcome.resulting_view, a).await;
    assert_eq!(combined.combines.len(), MAX_COMBINES);
    assert_eq!(combined.edits.len(), MAX_COMBINES + 2);
    fixture.validate().await;
}

/// Each side replaced the change's file with its own: the later replacement no longer
/// applies on top of the earlier one, and integration says which change.
#[tokio::test]
async fn revisions_that_do_not_combine_are_refused() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let left = revise(&fixture, base, first, a, Revision::Replace(b"left"), 1).await;
    let right = revise(&fixture, base, first, a, Revision::Replace(b"right"), 2).await;

    let error = fixture.integrate(base, &[left, right]).await.unwrap_err();

    assert!(
        matches!(&error, RepositoryMutationError::RevisionsDoNotCombine { change, .. } if *change == a),
        "{error}"
    );
}

/// One side revised a change the basis already holds, the other added work on top. The
/// joined view starts below the revised change and composes the basis's later change
/// again.
#[tokio::test]
async fn a_frontier_that_revises_its_basis_joins_from_below_the_change() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let (b, basis) = fixture.create(base, first, "b", b"b").await;
    let revised = revise(&fixture, base, basis, a, Revision::Add("x"), 1).await;
    let (c, added) = fixture.create(base, basis, "c", b"c").await;

    let outcome = fixture.integrate(basis, &[revised, added]).await.unwrap();

    assert_eq!(outcome.composed, [a, c]);
    assert_eq!(outcome.reapplied, [b]);
    assert!(outcome.combined.is_empty());
    assert!(outcome.conflicted.is_empty());
    let joined = outcome.resulting_view;
    for (path, bytes) in [
        ("a", b"a1".as_slice()),
        ("x", b"x"),
        ("b", b"b"),
        ("c", b"c"),
    ] {
        assert_eq!(fixture.file(joined, path).await, Some(ContentId::of(bytes)));
    }
    assert_eq!(fixture.frontiers(basis).await, [joined]);
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

    // Composed in canonical order, so revising a change in the middle keeps the rest.
    let later = revise(&fixture, base, joined, b, Revision::Add("y"), 2).await;
    for path in ["a", "x", "b", "c", "y"] {
        assert!(
            fixture.file(later, path).await.is_some(),
            "{path} is missing"
        );
    }
}

/// Revising a change the basis itself holds, as resolving a conflict in a label's view
/// does, composes from below the change. The result is still a frontier of the basis,
/// and the basis's changes are applied once.
#[tokio::test]
async fn revising_a_change_the_basis_holds_makes_a_frontier_of_it() {
    for first_in_order in [true, false] {
        let fixture = Fixture::new().await;
        let base = fixture.base;
        let (a, first) = fixture.create(base, base, "a", b"a1").await;
        let (b, basis) = fixture.create(base, first, "b", b"b").await;
        let revised = if first_in_order { a } else { b };

        let result = revise(&fixture, basis, basis, revised, Revision::Add("x"), 1).await;

        assert_eq!(fixture.frontiers(basis).await, [result], "{revised}");
        for path in ["a", "b", "x"] {
            assert!(
                fixture.file(result, path).await.is_some(),
                "{path} is missing"
            );
        }
        let view = fixture.reader().view(result).await.unwrap();
        let mut conflicts = ConflictSetStore::new(&fixture.objects)
            .cursor(view.unresolved_conflicts)
            .await
            .unwrap();
        assert!(conflicts.next().await.unwrap().is_none(), "{revised}");
    }
}

/// Both sides revised a change the basis holds: the revisions combine where the change
/// sits, below the basis's later change.
#[tokio::test]
async fn revisions_of_a_basis_change_combine_below_the_basis() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (a, first) = fixture.create(base, base, "a", b"a1").await;
    let (b, basis) = fixture.create(base, first, "b", b"b").await;
    let left = revise(&fixture, base, basis, a, Revision::Add("x"), 100).await;
    let right = revise(&fixture, base, basis, a, Revision::Add("y"), 200).await;

    let outcome = fixture.integrate(basis, &[left, right]).await.unwrap();

    assert_eq!(outcome.composed, [a]);
    assert_eq!(outcome.reapplied, [b]);
    assert_eq!(outcome.combined.keys().collect::<Vec<_>>(), [&a]);
    let joined = outcome.resulting_view;
    for path in ["a", "x", "y", "b"] {
        assert!(
            fixture.file(joined, path).await.is_some(),
            "{path} is missing"
        );
    }
    // Authored where `a` lands: the view below it, not the basis.
    assert_eq!(
        revision_in(&fixture, joined, a).await.authored_against,
        base
    );
    assert_ne!(first, base);
}

/// A change with no dependencies whose id sorts before the basis's change belongs below
/// it in canonical order. The joined view must be composed that way, or revising the
/// basis's change afterwards composes from a view that lacks the joined change.
#[tokio::test]
async fn a_joined_change_that_sorts_below_the_basis_is_kept_by_later_revisions() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let early = fixture.next_change();
    let (late, basis) = fixture.create(base, base, "late", b"late").await;
    fixture.content.put(b"early").unwrap();
    let (prepared, outcome) = fixture
        .mutator()
        .create(
            fixture.head(),
            base,
            basis,
            ChangeProposal {
                id: early,
                metadata: ChangeMetadata::default(),
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata::default(),
                edits: exact("early", None, b"early"),
            },
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    let with_early = outcome.resulting_view;
    let (_, beside) = fixture.create(base, basis, "beside", b"beside").await;

    let joined = fixture
        .integrate(basis, &[with_early, beside])
        .await
        .unwrap()
        .resulting_view;
    let revised = revise(&fixture, base, joined, late, Revision::Add("more"), 1).await;

    for path in ["early", "late", "beside", "more"] {
        assert!(
            fixture.file(revised, path).await.is_some(),
            "{path} is missing"
        );
    }
}

/// Joining one change on each side reads what the frontiers add, so a longer history
/// below the basis adds only the few radix levels it deepens.
#[tokio::test]
async fn integration_reads_what_the_frontiers_add() {
    let mut reads = Vec::new();
    for history in [24, 192] {
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

/// Joining a revision of a change near the top of the basis reads the changes from it
/// up, not the history below it.
#[tokio::test]
async fn integrating_a_revised_basis_change_reads_from_the_change_up() {
    let mut reads = Vec::new();
    for history in [24, 192] {
        let fixture = Fixture::new().await;
        let mut below = fixture.base;
        for index in 0..history {
            below = fixture
                .create(fixture.base, below, &format!("history/{index}"), b"h")
                .await
                .1;
        }
        let (a, first) = fixture.create(fixture.base, below, "a", b"a1").await;
        let (_, basis) = fixture.create(fixture.base, first, "b", b"b").await;
        let revised = revise(&fixture, fixture.base, basis, a, Revision::Add("x"), 1).await;
        let (_, added) = fixture.create(fixture.base, basis, "c", b"c").await;
        fixture.objects.reads.store(0, Ordering::Relaxed);
        let (_, outcome) = fixture
            .mutator()
            .integrate(fixture.head(), basis, &[revised, added])
            .await
            .unwrap();
        assert_eq!(outcome.composed.len(), 2);
        assert_eq!(outcome.reapplied.len(), 1);
        reads.push(fixture.objects.reads.load(Ordering::Relaxed));
    }
    assert!(
        reads[1] <= reads[0] + 16,
        "reads grew with the history: {reads:?}"
    );
}
