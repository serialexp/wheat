use std::{fs, str::FromStr};

use wheat_core::{ChangeId, ContentId, FileEdit, Repository, Transformation, Tree};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore, ObjectKind,
    PublicationError, PublicationStore, RepositoryPublicationError, RepositoryPublisher,
};

fn edit(path: &str, value: &[u8]) -> Transformation {
    Transformation {
        edits: vec![FileEdit {
            path: path.into(),
            expected: None,
            replacement: Some(ContentId::of(value)),
        }],
    }
}

fn stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

#[tokio::test]
async fn publishes_complete_canonical_closure_before_head() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change_with_id(
            ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("file", b"value"),
        )
        .unwrap();

    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let head = publisher.initialize(&repository).await.unwrap();
    let closure = repository.canonical_closure();

    assert_eq!(
        head.repository_state.as_bytes(),
        closure.state.id.as_bytes()
    );
    assert_eq!(
        objects
            .get_kind(head.repository_state, ObjectKind::RepositoryState)
            .await
            .unwrap()
            .canonical_bytes,
        closure.state.canonical_bytes()
    );
    assert!(closure.objects.len() >= 6);
    assert!(
        objects
            .exists_many(
                &closure
                    .objects
                    .iter()
                    .map(|object| object.id)
                    .collect::<Vec<_>>()
            )
            .await
            .unwrap()
            .into_iter()
            .all(|exists| exists)
    );
}

#[tokio::test]
async fn missing_required_content_prevents_head_creation_but_leaves_retryable_objects() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], edit("file", b"missing"))
        .unwrap();
    let closure = repository.canonical_closure();

    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let error = publisher.initialize(&repository).await.unwrap_err();
    assert!(
        matches!(error, RepositoryPublicationError::MissingContent(id) if id == ContentId::of(b"missing"))
    );
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
    assert!(
        objects
            .exists_many(
                &closure
                    .objects
                    .iter()
                    .map(|object| object.id)
                    .collect::<Vec<_>>()
            )
            .await
            .unwrap()
            .into_iter()
            .all(|exists| exists)
    );

    content.put(b"missing").unwrap();
    assert!(publisher.initialize(&repository).await.is_ok());
}

#[tokio::test]
async fn stale_publication_keeps_the_winning_head_and_retains_prepared_objects() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    content.put(b"first").unwrap();
    content.put(b"winner").unwrap();
    content.put(b"loser").unwrap();
    let mut base = Repository::new(Tree::new());
    base.create_change([], edit("first", b"first")).unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial = publisher.initialize(&base).await.unwrap();

    let mut winner = base.clone();
    winner.create_change([], edit("winner", b"winner")).unwrap();
    let winning_head = publisher.publish(initial, &winner).await.unwrap();

    let mut loser = base;
    loser.create_change([], edit("loser", b"loser")).unwrap();
    let losing_closure = loser.canonical_closure();
    assert!(matches!(
        publisher.publish(initial, &loser).await,
        Err(RepositoryPublicationError::Head(
            PublicationError::Stale { .. }
        ))
    ));
    assert_eq!(heads.read().unwrap(), winning_head);
    assert!(objects.get(losing_closure.state.id.into()).await.is_ok());
}

#[tokio::test]
async fn repeated_preparation_and_initialization_are_idempotent_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], edit("file", b"value"))
        .unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);

    let first = publisher.initialize(&repository).await.unwrap();
    let first_closure = publisher.prepare(&repository).await.unwrap();
    let second_closure = publisher.prepare(&repository).await.unwrap();
    assert_eq!(first_closure, second_closure);

    let (reopened_objects, reopened_content, reopened_heads) = stores(directory.path());
    let reopened = RepositoryPublisher::new(&reopened_objects, &reopened_content, &reopened_heads);
    assert_eq!(reopened.initialize(&repository).await.unwrap(), first);
    assert_eq!(reopened_heads.read().unwrap(), first);
}

#[tokio::test]
async fn corrupt_required_content_prevents_publication() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    let id = content.put(b"value").unwrap();
    fs::write(content.path(id), b"corrupt").unwrap();
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], edit("file", b"value"))
        .unwrap();

    let error = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap_err();
    assert!(matches!(error, RepositoryPublicationError::Content(_)));
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
}
