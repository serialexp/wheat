use std::{
    fs,
    str::FromStr,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ChangeId, ContentId, EditOperation, EditRecord, EditTransition, FileKind,
    FileValue, ObjectId, Repository, RevisionEdits, Tree,
};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore, ObjectKind,
    ObjectStoreError, PublicationError, PublicationStore, RepositoryPublicationError,
    RepositoryPublisher, RepositoryReader, StoredObject,
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

#[tokio::test]
async fn publishes_complete_canonical_closure_before_head() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    repository
        .create_change_with_id(
            basis,
            basis,
            ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("file", b"value"),
            ApplicationContext::new(&content),
        )
        .unwrap();

    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let head = publisher.initialize(&repository).await.unwrap();
    let closure = repository.canonical_closure();
    let envelope = RepositoryReader::new(&objects, &content)
        .envelope(head.envelope)
        .await
        .unwrap();

    assert_eq!(envelope.state.as_bytes(), closure.state.id.as_bytes());
    assert_eq!(
        objects
            .get_kind(envelope.state.into(), ObjectKind::RepositoryState)
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
    let authoring_content = ContentStore::new(directory.path().join("authoring-content"));
    authoring_content.put(b"missing").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    repository
        .create_change(
            basis,
            basis,
            [],
            edit("file", b"missing"),
            ApplicationContext::new(&authoring_content),
        )
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
    let basis = base.base_view().id;
    let (_, base_outcome) = base
        .create_change(
            basis,
            basis,
            [],
            edit("first", b"first"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let effective = base_outcome.resulting_view;
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial = publisher.initialize(&base).await.unwrap();

    let mut winner = base.clone();
    winner
        .create_change(
            basis,
            effective,
            [],
            edit("winner", b"winner"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let winning_head = publisher.publish(initial, &winner).await.unwrap();

    let mut loser = base;
    loser
        .create_change(
            basis,
            effective,
            [],
            edit("loser", b"loser"),
            ApplicationContext::new(&content),
        )
        .unwrap();
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
    let basis = repository.base_view().id;
    repository
        .create_change(
            basis,
            basis,
            [],
            edit("file", b"value"),
            ApplicationContext::new(&content),
        )
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

/// Counts how objects reach the store.
struct CountingStore {
    inner: LooseObjectStore,
    single_puts: AtomicUsize,
    batches: AtomicUsize,
}

#[async_trait]
impl ImmutableObjectStore for CountingStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.single_puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put_if_absent(id, bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[tokio::test]
async fn preparation_stores_the_closure_in_one_batch() {
    // A packed store commits each put on its own, with several syncs.
    let directory = tempfile::tempdir().unwrap();
    let (inner, content, heads) = stores(directory.path());
    let objects = CountingStore {
        inner,
        single_puts: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    };
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    repository
        .create_change(
            basis,
            basis,
            [],
            edit("file", b"value"),
            ApplicationContext::new(&content),
        )
        .unwrap();

    let closure = RepositoryPublisher::new(&objects, &content, &heads)
        .prepare(&repository)
        .await
        .unwrap();
    assert!(closure.objects.len() >= 6);
    assert_eq!(objects.single_puts.load(Ordering::Relaxed), 0);
    assert_eq!(objects.batches.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn corrupt_required_content_prevents_publication() {
    let directory = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(directory.path());
    let authoring_content = ContentStore::new(directory.path().join("authoring-content"));
    authoring_content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    repository
        .create_change(
            basis,
            basis,
            [],
            edit("file", b"value"),
            ApplicationContext::new(&authoring_content),
        )
        .unwrap();
    let id = content.put(b"value").unwrap();
    fs::write(content.path(id), b"corrupt").unwrap();

    let error = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap_err();
    assert!(matches!(error, RepositoryPublicationError::Content(_)));
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
}
