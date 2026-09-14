use std::collections::BTreeSet;

use wheat_core::*;
use wheat_store::*;

fn exact(path: &str, bytes: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(bytes),
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

fn proposal(id: ChangeId, path: &str, bytes: &[u8]) -> ChangeProposal {
    ChangeProposal {
        id,
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits: exact(path, bytes),
    }
}

#[tokio::test]
async fn buffered_prepare_flush_finalize_rejects_stale_loser_and_supports_reapply() {
    let dir = tempfile::tempdir().unwrap();
    let durable = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let initial = RepositoryPublisher::new(&durable, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    for bytes in [b"human".as_slice(), b"agent".as_slice()] {
        content.put(bytes).unwrap();
    }

    let batch = BatchObjectStore::new(&durable, 1_024, 4 * 1024 * 1024);
    let mutator = PersistedRepositoryMutator::new(&batch, &content, &heads);
    let human_id = ChangeId::from_bytes([1; 16]);
    let agent_id = ChangeId::from_bytes([2; 16]);
    let (human_mutation, human_outcome) = mutator
        .create(initial, base, base, proposal(human_id, "human", b"human"))
        .await
        .unwrap();
    let (agent_mutation, _) = mutator
        .create(initial, base, base, proposal(agent_id, "agent", b"agent"))
        .await
        .unwrap();

    let human = prepare_mutation_publication(&batch, &content, &heads, initial, human_mutation)
        .await
        .unwrap();
    let agent = prepare_mutation_publication(&batch, &content, &heads, initial, agent_mutation)
        .await
        .unwrap();
    assert_eq!(
        heads.read().unwrap(),
        initial,
        "prepare must not expose either actor"
    );
    assert!(
        durable.get(human.proposed().envelope.into()).await.is_err(),
        "envelopes remain buffered before flush"
    );

    batch.flush().await.unwrap();
    batch.flush().await.unwrap(); // An acknowledged flush is retry-safe.
    let winner = finalize_mutation_publication(&heads, human).unwrap();
    assert!(matches!(
        finalize_mutation_publication(&heads, agent),
        Err(RepositoryMutationError::Stale { expected, actual }) if expected == initial && actual == winner
    ));
    assert_eq!(heads.read().unwrap(), winner);

    // The losing agent reopens the winner and reapplies its independent intent.
    let winner_state = RepositoryReader::new(&durable, &content)
        .envelope(winner.envelope)
        .await
        .unwrap()
        .state;
    let reopened = RepositoryReader::new(&durable, &content)
        .hydrate(winner_state)
        .await
        .unwrap();
    assert_eq!(
        reopened.current_revision(human_outcome.resulting_view, human_id),
        Some(human_outcome.revision_id)
    );
    let effective = human_outcome.resulting_view;
    let retry_batch = BatchObjectStore::new(&durable, 1_024, 4 * 1024 * 1024);
    let retry_mutator = PersistedRepositoryMutator::new(&retry_batch, &content, &heads);
    let (retry, _) = retry_mutator
        .create(
            winner,
            base,
            effective,
            proposal(agent_id, "agent", b"agent"),
        )
        .await
        .unwrap();
    let retry = prepare_mutation_publication(&retry_batch, &content, &heads, winner, retry)
        .await
        .unwrap();
    retry_batch.flush().await.unwrap();
    let final_head = finalize_mutation_publication(&heads, retry).unwrap();

    let state = RepositoryReader::new(&durable, &content)
        .envelope(final_head.envelope)
        .await
        .unwrap()
        .state;
    let hydrated = RepositoryReader::new(&durable, &content)
        .hydrate(state)
        .await
        .unwrap();
    let closure = hydrated.canonical_closure();
    assert!(closure.state.change_objects.contains_key(&human_id));
    assert!(closure.state.change_objects.contains_key(&agent_id));
    hydrated.validate().unwrap();
}
