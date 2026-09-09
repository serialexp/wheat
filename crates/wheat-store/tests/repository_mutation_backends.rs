use std::path::Path;

use wheat_core::*;
use wheat_store::*;

const LARGE_LEN: usize = 128 * 1024 + 1;

#[derive(Debug, Eq, PartialEq)]
struct LifecycleTrace {
    states: Vec<RepositoryStateId>,
    envelopes: Vec<RepositoryEnvelopeId>,
    branch_manifest: BranchManifestId,
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

fn revised(path: &str, replacement: &[u8], superseded: EditId) -> RevisionEdits {
    let mut edits = exact(path, None, replacement);
    edits.transition.removed.insert(superseded);
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

async fn inspect<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    head: RepositoryHead,
    branches: BranchManifestId,
) -> RepositoryStateId {
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    assert_eq!(envelope.branches, branches);
    reader.validate_complete(envelope.state).await.unwrap();
    assert_eq!(
        reader
            .hydrate(envelope.state)
            .await
            .unwrap()
            .canonical_closure()
            .state
            .id,
        envelope.state
    );
    envelope.state
}

async fn lifecycle<O, F>(
    open: F,
    head_path: &Path,
    parent: ChangeId,
    child: ChangeId,
    winner_id: ChangeId,
    loser_id: ChangeId,
) -> LifecycleTrace
where
    O: ImmutableObjectStore,
    F: Fn() -> (O, ContentStore),
{
    let large = vec![b'L'; LARGE_LEN];
    let small = b"child-small".as_slice();
    let upstream = b"upstream".as_slice();
    let resolved = b"child-small".as_slice();

    // Seed content through the backend's public ContentStore adapter. The large
    // value exercises PackedObjectStore's default loose fallback.
    {
        let (_, content) = open();
        for bytes in [large.as_slice(), small, upstream, resolved] {
            content.put(bytes).unwrap();
        }
    }

    let branches;
    let mut head;
    {
        let (objects, content) = open();
        let heads = LocalPublicationStore::new(head_path);
        let repository = Repository::new(Tree::new());
        branches = BranchManifestRecord::new([
            ("refs/heads/main", repository.working_view().id),
            ("refs/heads/saved", repository.published_view().id),
        ])
        .unwrap();
        let publisher = RepositoryPublisher::new(&objects, &content, &heads);
        let closure = publisher.prepare(&repository).await.unwrap();
        let envelope = publisher
            .prepare_envelope(closure.state.id, &branches)
            .await
            .unwrap();
        head = publisher
            .initialize_prepared_envelope(envelope)
            .await
            .unwrap();
    }

    let mut states = Vec::new();
    let mut envelopes = vec![head.envelope];
    let parent_revision;
    let parent_edit;
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let parent_edits = exact("file", None, &large);
        parent_edit = *parent_edits.edits.keys().next().unwrap();
        let (prepared, parent_outcome) = mutator
            .create(head, proposal(parent, [], parent_edits))
            .await
            .unwrap();
        parent_revision = parent_outcome.revision_id;
        assert_eq!(heads.read().unwrap(), head, "prepare must not publish");
        head = mutator.publish(prepared).await.unwrap();
        envelopes.push(head.envelope);
    }

    // Reopen between every published transition, rather than merely checking
    // that the in-memory store instance can read its own writes.
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (prepared, _) = mutator
            .create(
                head,
                proposal(child, [parent], exact("file", Some(&large), small)),
            )
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        envelopes.push(head.envelope);
    }
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (prepared, outcome) = mutator
            .revise(
                head,
                parent,
                parent_revision,
                RevisionMetadata::default(),
                revised("file", upstream, parent_edit),
            )
            .await
            .unwrap();
        assert_eq!(outcome.reapplied, vec![child]);
        head = mutator.publish(prepared).await.unwrap();
        envelopes.push(head.envelope);
    }
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
        let repository = RepositoryReader::new(&objects, &content)
            .hydrate(*states.last().unwrap())
            .await
            .unwrap();
        let conflict_revision = repository.current_revision(child).unwrap();
        let application = repository
            .conflicted_application(conflict_revision)
            .expect("revising the dependency must conflict the child")
            .id;
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (prepared, _) = mutator
            .resolve_conflict(
                head,
                PersistedConflictResolution {
                    conflict_revision,
                    application,
                    resolved: [(
                        "file".into(),
                        Some(FileValue {
                            content: ContentId::of(resolved),
                            kind: FileKind::Regular,
                        }),
                    )]
                    .into(),
                },
            )
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        envelopes.push(head.envelope);
    }

    // Failure before publication: content resolution fails, no prepared value
    // is returned, and the durable head remains exactly unchanged.
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let failed = mutator
            .create(
                head,
                proposal(
                    ChangeId::new(),
                    [],
                    exact("missing", None, b"content-that-was-never-stored"),
                ),
            )
            .await;
        assert!(
            failed.is_err(),
            "missing content must fail before publication"
        );
        assert_eq!(heads.read().unwrap(), head);
        assert_eq!(
            inspect(&objects, &content, head, branches.id()).await,
            *states.last().unwrap()
        );
    }

    // Two valid preparations from one expected head are a representative CAS
    // race. Only the winner becomes visible; publishing or preparing via the
    // stale head cannot roll it back.
    {
        let (objects, content) = open();
        let heads = LocalPublicationStore::new(head_path);
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (winner, _) = mutator
            .create(head, proposal(winner_id, [], RevisionEdits::default()))
            .await
            .unwrap();
        let (loser, _) = mutator
            .create(head, proposal(loser_id, [], RevisionEdits::default()))
            .await
            .unwrap();
        let winner_head = mutator.publish(winner).await.unwrap();
        assert!(matches!(
            mutator.publish(loser).await,
            Err(RepositoryMutationError::Stale { .. })
        ));
        assert!(matches!(
            mutator
                .create(
                    head,
                    proposal(ChangeId::new(), [], RevisionEdits::default())
                )
                .await,
            Err(RepositoryMutationError::Stale { .. })
        ));
        assert_eq!(heads.read().unwrap(), winner_head);
        head = winner_head;
        envelopes.push(head.envelope);
    }
    {
        let (objects, content) = open();
        states.push(inspect(&objects, &content, head, branches.id()).await);
    }

    LifecycleTrace {
        states,
        envelopes,
        branch_manifest: branches.id(),
    }
}

#[tokio::test]
async fn loose_and_packed_mutation_lifecycles_are_identical_and_atomic() {
    let loose = tempfile::tempdir().unwrap();
    let packed = tempfile::tempdir().unwrap();
    let parent = ChangeId::new();
    let child = ChangeId::new();
    let winner = ChangeId::new();
    let loser = ChangeId::new();

    let loose_objects = loose.path().join("objects");
    let loose_content = loose.path().join("content");
    let loose_trace = lifecycle(
        || {
            (
                LooseObjectStore::new(&loose_objects),
                ContentStore::new(&loose_content),
            )
        },
        &loose.path().join("head"),
        parent,
        child,
        winner,
        loser,
    )
    .await;

    let packed_root = packed.path().join("store");
    let packed_trace = lifecycle(
        || {
            let objects = PackedObjectStore::open(&packed_root).unwrap();
            let content = objects.content_store();
            (objects, content)
        },
        &packed.path().join("head"),
        parent,
        child,
        winner,
        loser,
    )
    .await;

    assert_eq!(packed_trace, loose_trace);

    let packed_objects = PackedObjectStore::open(&packed_root).unwrap();
    let packed_content = packed_objects.content_store();
    let large_id = ContentId::of(&vec![b'L'; LARGE_LEN]);
    let small_id = ContentId::of(b"child-small");
    assert!(
        packed_content.path(large_id).is_file(),
        ">128 KiB content must use loose fallback"
    );
    assert!(
        !packed_content.path(small_id).exists(),
        "small content should remain packed"
    );
    assert_eq!(packed_content.get(large_id).unwrap().len(), LARGE_LEN);
    assert_eq!(packed_content.get(small_id).unwrap(), b"child-small");
}
