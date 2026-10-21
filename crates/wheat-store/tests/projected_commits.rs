//! The Git commits a repository state records its applications as projecting to.

use std::collections::BTreeSet;

use wheat_core::*;
use wheat_store::*;

fn exact(path: &str, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
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
            ..Default::default()
        },
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
}

/// A published change, revised once, above another change.
struct Published {
    head: RepositoryHead,
    base: ViewId,
    view: ViewId,
    change: ChangeId,
    revision: RevisionId,
    /// The applications of the first and of the second revision.
    applications: [ApplicationId; 2],
    /// The change below it, its revision and application.
    below: (ChangeId, RevisionId, ApplicationId),
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        for bytes in [b"one".as_slice(), b"two"] {
            content.put(bytes).unwrap();
        }
        Self {
            _dir: dir,
            objects,
            content,
            heads,
        }
    }

    fn reader(&self) -> RepositoryReader<'_, LooseObjectStore> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    fn mutator(&self) -> PersistedRepositoryMutator<'_, LooseObjectStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    fn projected(&self) -> ProjectedCommitSetStore<'_, LooseObjectStore> {
        ProjectedCommitSetStore::new(&self.objects)
    }

    async fn state(&self, head: RepositoryHead) -> RepositoryStateRecord {
        let reader = self.reader();
        let envelope = reader.envelope(head.envelope).await.unwrap();
        reader.state(envelope.state).await.unwrap()
    }

    async fn applications(&self, head: RepositoryHead) -> BTreeSet<ApplicationId> {
        let state = self.state(head).await;
        let mut cursor = RetainedSetStore::new(&self.objects)
            .cursor(state.retained_applications)
            .await
            .unwrap();
        let mut applications = BTreeSet::new();
        while let Some(key) = cursor.next().await.unwrap() {
            applications.insert(ApplicationId::from_digest(*key.as_bytes()));
        }
        applications
    }

    async fn members(&self, root: ProjectedCommitSetId) -> BTreeSet<ProjectedCommit> {
        let mut cursor = self.projected().cursor(root).await.unwrap();
        let mut members = BTreeSet::new();
        while let Some(member) = cursor.next().await.unwrap() {
            members.insert(member);
        }
        members
    }

    async fn publish(&self) -> Published {
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let mut head = RepositoryPublisher::new(&self.objects, &self.content, &self.heads)
            .initialize(&repository)
            .await
            .unwrap();
        let initial = self.applications(head).await;
        let mutator = self.mutator();
        let below_change = ChangeId::new();
        let (prepared, below) = mutator
            .create(
                head,
                base,
                base,
                ChangeProposal {
                    id: below_change,
                    metadata: Default::default(),
                    dependencies: BTreeSet::new(),
                    revision_metadata: Default::default(),
                    edits: exact("below", b"one"),
                },
            )
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        let before = self.applications(head).await;
        let change = ChangeId::new();
        let first_edits = exact("file", b"one");
        let first_edit = *first_edits.edits.keys().next().unwrap();
        let (prepared, first) = mutator
            .create(
                head,
                base,
                below.resulting_view,
                ChangeProposal {
                    id: change,
                    metadata: Default::default(),
                    dependencies: BTreeSet::new(),
                    revision_metadata: Default::default(),
                    edits: first_edits,
                },
            )
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        let after_first = self.applications(head).await;
        let mut second_edits = exact("file", b"two");
        second_edits.transition.removed.insert(first_edit);
        let (prepared, second) = mutator
            .revise(
                head,
                base,
                first.resulting_view,
                change,
                first.revision_id,
                Default::default(),
                second_edits,
            )
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        let after_second = self.applications(head).await;
        let only = |set: BTreeSet<ApplicationId>| {
            let mut set = set.into_iter();
            let application = set.next().expect("one new application");
            assert!(set.next().is_none(), "one new application");
            application
        };
        Published {
            head,
            base,
            view: second.resulting_view,
            change,
            revision: second.revision_id,
            applications: [
                only(&after_first - &before),
                only(&after_second - &after_first),
            ],
            below: (below_change, below.revision_id, only(&before - &initial)),
        }
    }
}

fn commit(byte: u8) -> [u8; 20] {
    [byte; ProjectedCommit::COMMIT_LEN]
}

#[tokio::test]
async fn recording_adds_members_without_touching_views_and_validates() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let before = fixture.state(published.head).await;
    let members = [
        ProjectedCommit::new(commit(1), published.applications[0]),
        ProjectedCommit::new(commit(2), published.applications[1]),
    ];

    let prepared = fixture
        .mutator()
        .record_projected_commits(published.head, published.view, members)
        .await
        .unwrap()
        .expect("new members");
    assert_eq!(prepared.resulting_view(), published.view);
    let head = fixture.mutator().publish(prepared).await.unwrap();

    let after = fixture.state(head).await;
    assert_eq!(
        fixture.members(after.projected_commits).await,
        members.into()
    );
    assert_eq!(
        RepositoryStateRecord {
            projected_commits: before.projected_commits,
            ..after
        },
        before,
        "only the projected commits change"
    );
    let reader = fixture.reader();
    reader.validate_complete(after.id()).await.unwrap();
    reader
        .validate_transition(before.id(), after.id())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .projected()
            .applications(after.projected_commits, commit(2))
            .await
            .unwrap(),
        [published.applications[1]]
    );
    assert!(
        fixture
            .projected()
            .applications(after.projected_commits, commit(3))
            .await
            .unwrap()
            .is_empty()
    );

    // Recording what the state holds already writes nothing.
    assert!(
        fixture
            .mutator()
            .record_projected_commits(head, published.view, [members[0]])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn one_commit_may_project_several_applications() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let members = published
        .applications
        .map(|application| ProjectedCommit::new(commit(7), application));

    let prepared = fixture
        .mutator()
        .record_projected_commits(published.head, published.view, members)
        .await
        .unwrap()
        .unwrap();

    let state = fixture.reader().state(prepared.state()).await.unwrap();
    let mut applications = published.applications.to_vec();
    applications.sort();
    assert_eq!(
        fixture
            .projected()
            .applications(state.projected_commits, commit(7))
            .await
            .unwrap(),
        applications
    );
}

#[tokio::test]
async fn recording_an_application_the_state_does_not_retain_is_refused() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let unknown = ApplicationId::from_digest([9; 32]);

    let error = fixture
        .mutator()
        .record_projected_commits(
            published.head,
            published.view,
            [ProjectedCommit::new(commit(1), unknown)],
        )
        .await
        .err()
        .expect("refused");

    assert!(
        error
            .to_string()
            .contains("projected commit names an application the state does not retain"),
        "{error}"
    );
}

#[tokio::test]
async fn recording_against_a_stale_head_is_refused() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let member = ProjectedCommit::new(commit(1), published.applications[1]);
    let prepared = fixture
        .mutator()
        .record_projected_commits(published.head, published.view, [member])
        .await
        .unwrap()
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();

    let error = fixture
        .mutator()
        .record_projected_commits(published.head, published.view, [member])
        .await
        .err()
        .expect("refused");

    assert!(
        matches!(error, RepositoryMutationError::Stale { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn validation_refuses_a_member_whose_application_is_not_retained() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let state = fixture.state(published.head).await;
    let forged = RepositoryStateRecord {
        projected_commits: fixture
            .projected()
            .insert(
                state.projected_commits,
                ProjectedCommit::new(commit(1), ApplicationId::from_digest([9; 32])),
            )
            .await
            .unwrap(),
        ..state
    };
    fixture
        .objects
        .put_if_absent(forged.id().into(), &forged.canonical_bytes())
        .await
        .unwrap();

    let reader = fixture.reader();
    assert!(reader.validate_complete(forged.id()).await.is_err());
    assert!(
        reader
            .validate_transition(state.id(), forged.id())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropping_a_member_is_a_non_monotonic_transition() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let before = fixture.state(published.head).await;
    let prepared = fixture
        .mutator()
        .record_projected_commits(
            published.head,
            published.view,
            [ProjectedCommit::new(commit(1), published.applications[1])],
        )
        .await
        .unwrap()
        .unwrap();

    let reader = fixture.reader();
    assert!(
        reader
            .validate_transition(prepared.state(), before.id())
            .await
            .is_err()
    );
    reader
        .validate_transition_with_options(
            prepared.state(),
            before.id(),
            TransitionValidationOptions {
                allow_non_monotonic: true,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_union_holds_both_sides_members() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let here = ProjectedCommit::new(commit(1), published.applications[0]);
    let there = ProjectedCommit::new(commit(2), published.applications[1]);
    let record = |member| {
        let fixture = &fixture;
        let published = &published;
        async move {
            fixture
                .mutator()
                .record_projected_commits(published.head, published.view, [member])
                .await
                .unwrap()
                .unwrap()
                .state()
        }
    };
    let local = record(here).await;
    let remote = record(there).await;

    let reader = fixture.reader();
    let union = reader.union_states(local, remote).await.unwrap();

    let state = reader.state(union).await.unwrap();
    assert_eq!(
        fixture.members(state.projected_commits).await,
        [here, there].into()
    );
    reader.validate_complete(union).await.unwrap();
}

#[tokio::test]
async fn a_squash_drops_the_members_of_applications_it_drops() {
    let fixture = Fixture::new();
    let published = fixture.publish().await;
    let (below_change, below_revision, below) = published.below;
    let kept = ProjectedCommit::new(commit(2), below);
    let members = published
        .applications
        .map(|application| ProjectedCommit::new(commit(1), application));
    let prepared = fixture
        .mutator()
        .record_projected_commits(
            published.head,
            published.view,
            members.into_iter().chain([kept]),
        )
        .await
        .unwrap()
        .unwrap();
    let head = fixture.mutator().publish(prepared).await.unwrap();

    let request = SquashRewriteRequest {
        expected_revisions: [
            (published.change, published.revision),
            (below_change, below_revision),
        ]
        .into(),
        retained_predecessors: [(published.change, None)].into(),
        revision_metadata: [(published.change, RevisionMetadata::default())].into(),
        authority_roots: [published.base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&fixture.objects, &fixture.content, &fixture.heads)
        .prepare_squash(head, published.base, published.view, request)
        .await
        .unwrap()
        .unwrap();
    let squashed = finalize_repository_rewrite(&fixture.heads, prepared).unwrap();

    let retained = fixture.applications(squashed).await;
    for application in published.applications {
        assert!(!retained.contains(&application), "squash replaced it");
    }
    assert!(retained.contains(&below), "squash left it alone");
    let state = fixture.state(squashed).await;
    assert_eq!(
        fixture.members(state.projected_commits).await,
        [kept].into()
    );
    fixture
        .reader()
        .validate_complete(state.id())
        .await
        .unwrap();
}
