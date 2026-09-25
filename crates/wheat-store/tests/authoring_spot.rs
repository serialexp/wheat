//! Every revision is authored at its change's spot: the view after every change before it in
//! canonical order. That includes earlier siblings the change never declared as dependencies.

use std::collections::{BTreeMap, BTreeSet};

use wheat_core::*;
use wheat_store::*;

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

/// `next` added on top of every edit of `previous`, which it keeps unchanged.
fn keeping(previous: &RevisionEdits, mut next: RevisionEdits) -> RevisionEdits {
    for (id, edit) in &previous.edits {
        next.edits.insert(*id, edit.clone());
        next.transition.preserved.insert(*id);
    }
    next
}

fn proposal(
    id: ChangeId,
    dependencies: impl IntoIterator<Item = ChangeId>,
    edits: RevisionEdits,
) -> ChangeProposal {
    ChangeProposal {
        id,
        metadata: ChangeMetadata::default(),
        dependencies: dependencies.into_iter().collect(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture {
        objects: LooseObjectStore::new(dir.path().join("objects")),
        content: ContentStore::new(dir.path().join("content")),
        heads: LocalPublicationStore::new(dir.path().join("head")),
        _dir: dir,
    };
    for bytes in [
        b"s1".as_slice(),
        b"s2",
        b"b1",
        b"c1",
        b"a-extra",
        b"b-extra",
    ] {
        fixture.content.put(bytes).unwrap();
    }
    fixture
}

/// The view `change` is applied onto in `view`.
async fn spot(fixture: &Fixture, view: ViewId, change: ChangeId) -> ViewId {
    let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
    let view = reader.view(view).await.unwrap();
    let application = ApplicationMapStore::new(&fixture.objects)
        .get(view.applications, change)
        .await
        .unwrap()
        .unwrap();
    reader.application(application).await.unwrap().target_view
}

#[tokio::test]
async fn store_and_reference_author_revisions_at_their_spot() {
    let fixture = fixture();
    let context = ApplicationContext::new(&fixture.content);
    let mut reference = Repository::new(Tree::new());
    let base = reference.base_view().id;
    // A and B are independent siblings. Change ids grow with time, so A comes first.
    let a = ChangeId::new();
    let a1_edits = exact("shared", None, b"s1");
    let a1 = reference
        .create_change_with_id(base, base, a, [], a1_edits.clone(), context)
        .unwrap();
    let b = ChangeId::new();
    let b1_edits = exact("b", None, b"b1");
    let b1 = reference
        .create_change_with_id(base, a1.resulting_view, b, [], b1_edits.clone(), context)
        .unwrap();
    assert!(a < b);
    let mut effective = b1.resulting_view;
    let publisher = RepositoryPublisher::new(&fixture.objects, &fixture.content, &fixture.heads);
    let mut head = publisher.initialize(&reference).await.unwrap();
    let mutator =
        PersistedRepositoryMutator::new(&fixture.objects, &fixture.content, &fixture.heads);
    let reader = RepositoryReader::new(&fixture.objects, &fixture.content);

    // B edits the file A created. B never declared A, but it sits on top of it.
    let b2_edits = keeping(&b1_edits, exact("shared", Some(b"s1"), b"s2"));
    let expected = reference
        .revise_change(
            base,
            effective,
            b,
            b1.revision_id,
            b2_edits.clone(),
            context,
        )
        .unwrap();
    let (prepared, actual) = mutator
        .revise(
            head,
            base,
            effective,
            b,
            b1.revision_id,
            RevisionMetadata::default(),
            b2_edits.clone(),
        )
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(prepared.state(), reference.canonical_closure().state.id);
    head = mutator.publish(prepared).await.unwrap();
    effective = actual.resulting_view;
    let b2 = reader.revision(actual.revision_id).await.unwrap();
    assert_eq!(b2.authored_against, spot(&fixture, effective, b).await);
    assert_eq!(b2.authored_against, a1.resulting_view);

    // C declares only A but is created after B, so it sits after B and may edit B's file.
    let c = ChangeId::new();
    let c_edits = exact("b", Some(b"b1"), b"c1");
    let expected = reference
        .create_change_with_metadata(
            base,
            effective,
            c,
            ChangeMetadata::default(),
            [a],
            RevisionMetadata::default(),
            c_edits.clone(),
            context,
        )
        .unwrap();
    let (prepared, actual) = mutator
        .create(head, base, effective, proposal(c, [a], c_edits))
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(prepared.state(), reference.canonical_closure().state.id);
    head = mutator.publish(prepared).await.unwrap();
    effective = actual.resulting_view;
    let c1 = reader.revision(actual.revision_id).await.unwrap();
    assert_eq!(c1.authored_against, spot(&fixture, effective, c).await);

    // Revising A and B together publishes once. B is authored on top of A's new revision.
    let requests = BTreeMap::from([
        (
            a,
            RevisionRequest {
                predecessor: a1.revision_id,
                metadata: RevisionMetadata::default(),
                edits: keeping(&a1_edits, exact("a-extra", None, b"a-extra")),
            },
        ),
        (
            b,
            RevisionRequest {
                predecessor: b2.id(),
                metadata: RevisionMetadata::default(),
                edits: keeping(&b2_edits, exact("b-extra", None, b"b-extra")),
            },
        ),
    ]);
    let expected = reference
        .revise_changes_with_metadata(base, effective, requests.clone(), context)
        .unwrap();
    let (prepared, actual) = mutator
        .revise_changes(head, base, effective, requests)
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.reapplied, vec![c]);
    assert_eq!(prepared.state(), reference.canonical_closure().state.id);
    let published = mutator.publish(prepared).await.unwrap();
    assert_eq!(published.generation, head.generation + 1);
    effective = actual.resulting_view;
    let new_a = actual.revisions[&a];
    let new_b = reader.revision(actual.revisions[&b]).await.unwrap();
    assert_eq!(new_b.authored_against, spot(&fixture, effective, b).await);
    assert_eq!(
        reader.revision(new_a).await.unwrap().resulting_tree,
        reader.view(new_b.authored_against).await.unwrap().tree
    );
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_keeps_a_change_that_relies_on_an_earlier_sibling() {
    let fixture = fixture();
    let heads = &fixture.heads;
    let reference = Repository::new(Tree::new());
    let base = reference.base_view().id;
    let mut head = RepositoryPublisher::new(&fixture.objects, &fixture.content, heads)
        .initialize(&reference)
        .await
        .unwrap();
    let mutator = PersistedRepositoryMutator::new(&fixture.objects, &fixture.content, heads);
    let a = ChangeId::new();
    let (prepared, a1) = mutator
        .create(
            head,
            base,
            base,
            proposal(a, [], exact("shared", None, b"s1")),
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let b = ChangeId::new();
    let b1_edits = exact("b", None, b"b1");
    let (prepared, b1) = mutator
        .create(
            head,
            base,
            a1.resulting_view,
            proposal(b, [], b1_edits.clone()),
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let (prepared, b2) = mutator
        .revise(
            head,
            base,
            b1.resulting_view,
            b,
            b1.revision_id,
            RevisionMetadata::default(),
            keeping(&b1_edits, exact("shared", Some(b"s1"), b"s2")),
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let effective = b2.resulting_view;

    // Collapsing B's history keeps its edits. They only apply on top of A, B's spot.
    let request = SquashRewriteRequest {
        expected_revisions: [(a, a1.revision_id), (b, b2.revision_id)].into(),
        retained_predecessors: [(b, None)].into(),
        revision_metadata: [(b, RevisionMetadata::default())].into(),
        authority_roots: [base, effective].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&fixture.objects, &fixture.content, heads)
        .prepare_squash(head, base, effective, request)
        .await
        .unwrap()
        .unwrap();
    let squashed = prepared.mapping().revisions[&b2.revision_id];
    let resulting = prepared.resulting_view();
    let published = finalize_repository_rewrite(heads, prepared).unwrap();
    let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
    let squashed = reader.revision(squashed).await.unwrap();
    assert_eq!(squashed.predecessor, None);
    assert_eq!(
        squashed.authored_against,
        spot(&fixture, resulting, b).await
    );
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}
