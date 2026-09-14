use std::collections::BTreeSet;

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

fn revised(
    path: &str,
    expected: Option<&[u8]>,
    replacement: &[u8],
    removed: EditId,
) -> RevisionEdits {
    let mut edits = exact(path, expected, replacement);
    edits.transition.removed.insert(removed);
    edits
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
    base: ViewId,
    head: RepositoryHead,
}

async fn fixture(initial: &[&[u8]]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in initial {
        content.put(bytes).unwrap();
    }
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        objects,
        content,
        heads,
        base,
        head,
    }
}

#[tokio::test]
async fn human_and_agent_independent_simultaneous_preparations_have_one_cas_winner() {
    let f = fixture(&[b"human", b"agent"]).await;
    let mutator = PersistedRepositoryMutator::new(&f.objects, &f.content, &f.heads);
    let (human, _) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(
                ChangeId::from_bytes([1; 16]),
                [],
                exact("human", None, b"human"),
            ),
        )
        .await
        .unwrap();
    let (agent, _) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(
                ChangeId::from_bytes([2; 16]),
                [],
                exact("agent", None, b"agent"),
            ),
        )
        .await
        .unwrap();
    let human_head = mutator.publish(human).await.unwrap();
    assert!(
        matches!(mutator.publish(agent).await, Err(RepositoryMutationError::Stale { expected, actual }) if expected == f.head && actual == human_head)
    );
    assert_eq!(f.heads.read().unwrap(), human_head);
}

#[tokio::test]
async fn losing_agent_reopens_and_reapplies_independent_mutation_after_human_wins() {
    let f = fixture(&[b"human", b"agent"]).await;
    let mutator = PersistedRepositoryMutator::new(&f.objects, &f.content, &f.heads);
    let human_id = ChangeId::from_bytes([3; 16]);
    let agent_id = ChangeId::from_bytes([4; 16]);
    let (human, human_outcome) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(human_id, [], exact("human", None, b"human")),
        )
        .await
        .unwrap();
    let (agent, _) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(agent_id, [], exact("agent", None, b"agent")),
        )
        .await
        .unwrap();
    let human_head = mutator.publish(human).await.unwrap();
    assert!(matches!(
        mutator.publish(agent).await,
        Err(RepositoryMutationError::Stale { .. })
    ));
    let (reapplied, outcome) = mutator
        .create(
            human_head,
            f.base,
            human_outcome.resulting_view,
            proposal(agent_id, [], exact("agent", None, b"agent")),
        )
        .await
        .unwrap();
    let final_head = mutator.publish(reapplied).await.unwrap();
    let state = RepositoryReader::new(&f.objects, &f.content)
        .envelope(final_head.envelope)
        .await
        .unwrap()
        .state;
    let repository = RepositoryReader::new(&f.objects, &f.content)
        .hydrate(state)
        .await
        .unwrap();
    assert_eq!(
        repository.view(outcome.resulting_view).unwrap().tree.len(),
        2
    );
}

#[tokio::test]
async fn competing_human_and_agent_resolutions_refuse_the_stale_loser() {
    let f = fixture(&[b"base", b"human", b"agent", b"upstream"]).await;
    let mutator = PersistedRepositoryMutator::new(&f.objects, &f.content, &f.heads);
    let parent = ChangeId::from_bytes([5; 16]);
    let child = ChangeId::from_bytes([6; 16]);
    let (p, po) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(parent, [], exact("shared", None, b"base")),
        )
        .await
        .unwrap();
    let mut head = mutator.publish(p).await.unwrap();
    let parent_edit = *exact("shared", None, b"base").edits.keys().next().unwrap();
    let (c, co) = mutator
        .create(
            head,
            f.base,
            po.resulting_view,
            proposal(child, [parent], exact("shared", Some(b"base"), b"human")),
        )
        .await
        .unwrap();
    head = mutator.publish(c).await.unwrap();
    let (moved, moved_outcome) = mutator
        .revise(
            head,
            f.base,
            co.resulting_view,
            parent,
            po.revision_id,
            RevisionMetadata::default(),
            revised("shared", None, b"upstream", parent_edit),
        )
        .await
        .unwrap();
    head = mutator.publish(moved).await.unwrap();
    let state = RepositoryReader::new(&f.objects, &f.content)
        .envelope(head.envelope)
        .await
        .unwrap()
        .state;
    let repository = RepositoryReader::new(&f.objects, &f.content)
        .hydrate(state)
        .await
        .unwrap();
    let conflict_revision = repository
        .current_revision(moved_outcome.resulting_view, child)
        .unwrap();
    let application = repository
        .conflicted_application(moved_outcome.resulting_view, conflict_revision)
        .unwrap()
        .id;
    let resolution = |bytes: &[u8]| PersistedConflictResolution {
        conflict_revision,
        application,
        resolved: [(
            "shared".into(),
            Some(FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
        )]
        .into(),
    };
    let (human, _) = mutator
        .resolve_conflict(
            head,
            f.base,
            moved_outcome.resulting_view,
            resolution(b"human"),
        )
        .await
        .unwrap();
    let (agent, _) = mutator
        .resolve_conflict(
            head,
            f.base,
            moved_outcome.resulting_view,
            resolution(b"agent"),
        )
        .await
        .unwrap();
    let winner = mutator.publish(human).await.unwrap();
    assert!(
        matches!(mutator.publish(agent).await, Err(RepositoryMutationError::Stale { actual, .. }) if actual == winner)
    );
}

#[tokio::test]
async fn dependency_movement_makes_agent_publication_stale_then_retry_reapplies_descendant() {
    let f = fixture(&[b"p1", b"p2", b"child", b"human"]).await;
    let mutator = PersistedRepositoryMutator::new(&f.objects, &f.content, &f.heads);
    let parent = ChangeId::from_bytes([7; 16]);
    let child = ChangeId::from_bytes([8; 16]);
    let (p, po) = mutator
        .create(
            f.head,
            f.base,
            f.base,
            proposal(parent, [], exact("parent", None, b"p1")),
        )
        .await
        .unwrap();
    let mut head = mutator.publish(p).await.unwrap();
    let old = *exact("parent", None, b"p1").edits.keys().next().unwrap();
    let (c, co) = mutator
        .create(
            head,
            f.base,
            po.resulting_view,
            proposal(child, [parent], exact("child", None, b"child")),
        )
        .await
        .unwrap();
    head = mutator.publish(c).await.unwrap();
    let (agent_move, _) = mutator
        .revise(
            head,
            f.base,
            co.resulting_view,
            parent,
            po.revision_id,
            RevisionMetadata::default(),
            revised("parent", None, b"p2", old),
        )
        .await
        .unwrap();
    let (human, human_outcome) = mutator
        .create(
            head,
            f.base,
            co.resulting_view,
            proposal(
                ChangeId::from_bytes([9; 16]),
                BTreeSet::new(),
                exact("human", None, b"human"),
            ),
        )
        .await
        .unwrap();
    let human_head = mutator.publish(human).await.unwrap();
    assert!(matches!(
        mutator.publish(agent_move).await,
        Err(RepositoryMutationError::Stale { .. })
    ));
    let (retry, outcome) = mutator
        .revise(
            human_head,
            f.base,
            human_outcome.resulting_view,
            parent,
            po.revision_id,
            RevisionMetadata::default(),
            revised("parent", None, b"p2", old),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reapplied, vec![child]);
    let final_head = mutator.publish(retry).await.unwrap();

    // Reopen from the publication boundary rather than trusting the in-memory mutation outcome.
    let reader = RepositoryReader::new(&f.objects, &f.content);
    let envelope = reader.envelope(final_head.envelope).await.unwrap();
    let repository = reader.hydrate(envelope.state).await.unwrap();
    let parent_revision = repository
        .current_revision(outcome.resulting_view, parent)
        .unwrap();
    let child_revision = repository
        .current_revision(outcome.resulting_view, child)
        .unwrap();
    let parent_record = reader.revision(parent_revision).await.unwrap();
    let child_record = reader.revision(child_revision).await.unwrap();
    assert_eq!(child_record.dependencies, [parent].into());
    assert_ne!(
        child_record.authored_against,
        parent_record.authored_against
    );
    let authored = repository.view(child_record.authored_against).unwrap();
    assert!(authored.tree.contains_key("parent"));
    let view = repository.view(outcome.resulting_view).unwrap();
    assert_eq!(
        view.tree.get("parent").map(|value| value.content),
        Some(ContentId::of(b"p2"))
    );
    assert_eq!(
        view.tree.get("child").map(|value| value.content),
        Some(ContentId::of(b"child"))
    );
    assert_eq!(
        view.tree.get("human").map(|value| value.content),
        Some(ContentId::of(b"human"))
    );
    let application = repository
        .current_application(outcome.resulting_view, child)
        .unwrap();
    assert_eq!(application.revision_id, child_revision);
    assert_eq!(
        application
            .result
            .materialized_tree()
            .get("parent")
            .map(|value| value.content),
        Some(ContentId::of(b"p2"))
    );
    assert_eq!(
        application
            .result
            .materialized_tree()
            .get("child")
            .map(|value| value.content),
        Some(ContentId::of(b"child"))
    );
}
