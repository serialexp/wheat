use std::str::FromStr;

use wheat_core::{
    ApplicationContext, ChangeId, ContentId, EditOperation, EditRecord, EditTransition, FileKind,
    FileValue, Repository, RepositoryStateId, RevisionEdits, Tree,
};
use wheat_store::{
    ContentStore, LocalPublicationStore, LooseObjectStore, RepositoryPublisher, RepositoryReader,
    TransitionValidationOptions,
};

fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

fn stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

/// Build a repository with one change, publish it, add a second change, publish
/// that. Returns the two state IDs for the two published states.
async fn two_state_repository(
    objects: &LooseObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
) -> (RepositoryStateId, RepositoryStateId) {
    content.put(b"first").unwrap();
    content.put(b"second").unwrap();

    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;

    // First change.
    let first_outcome = repository
        .create_change_with_id(
            basis,
            basis,
            ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("file_a.txt", b"first"),
            ApplicationContext::new(content),
        )
        .unwrap();
    let effective = first_outcome.resulting_view;

    let publisher = RepositoryPublisher::new(objects, content, heads);
    let first_head = publisher.initialize(&repository).await.unwrap();
    let first_envelope = RepositoryReader::new(objects, content)
        .envelope(first_head.envelope)
        .await
        .unwrap();
    let first_state = first_envelope.state;

    // Second change.
    repository
        .create_change_with_id(
            effective,
            effective,
            ChangeId::from_str("02890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("file_b.txt", b"second"),
            ApplicationContext::new(content),
        )
        .unwrap();

    let second_head = publisher.publish(first_head, &repository).await.unwrap();
    let second_envelope = RepositoryReader::new(objects, content)
        .envelope(second_head.envelope)
        .await
        .unwrap();
    let second_state = second_envelope.state;

    (first_state, second_state)
}

#[tokio::test]
async fn valid_monotonic_transition_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(dir.path());
    let (first_state, second_state) = two_state_repository(&objects, &content, &heads).await;

    let reader = RepositoryReader::new(&objects, &content);
    reader
        .validate_transition(first_state, second_state)
        .await
        .unwrap();
}

#[tokio::test]
async fn identical_states_validate_trivially() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(dir.path());
    let (first_state, _) = two_state_repository(&objects, &content, &heads).await;

    let reader = RepositoryReader::new(&objects, &content);
    reader
        .validate_transition(first_state, first_state)
        .await
        .unwrap();
}

#[tokio::test]
async fn reverse_transition_rejected_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(dir.path());
    let (first_state, second_state) = two_state_repository(&objects, &content, &heads).await;

    let reader = RepositoryReader::new(&objects, &content);
    let result = reader.validate_transition(second_state, first_state).await;
    assert!(
        result.is_err(),
        "reverse transition should fail without force"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("drops"),
        "error should mention dropping: {err}"
    );
}

#[tokio::test]
async fn reverse_transition_allowed_with_force() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(dir.path());
    let (first_state, second_state) = two_state_repository(&objects, &content, &heads).await;

    let reader = RepositoryReader::new(&objects, &content);
    reader
        .validate_transition_with_options(
            second_state,
            first_state,
            TransitionValidationOptions {
                allow_non_monotonic: true,
            },
        )
        .await
        .unwrap();
}
