use std::sync::Arc;

use object_store::{ObjectStore as BackendObjectStore, memory::InMemory};
use wheat_core::*;
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, ObjectStoreAdapter,
    ObjectStoreError, PublicationStore, RepositoryPublisher, RepositoryReader, S3ObjectStoreConfig,
};

fn revision_edits(path: &str, value: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
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
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..Default::default()
        },
    }
}

fn canonical_objects() -> (Vec<u8>, Vec<u8>) {
    let content = wheat_store::ContentStore::new(tempfile::tempdir().unwrap().keep());
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    let (_, outcome) = repository
        .create_change(
            basis,
            basis,
            [],
            revision_edits("file", b"value"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    (
        repository
            .revision(outcome.revision_id)
            .unwrap()
            .canonical_bytes(),
        repository
            .view(outcome.resulting_view)
            .unwrap()
            .canonical_bytes(),
    )
}

async fn run_conformance(store: &dyn ImmutableObjectStore) {
    let (revision, view) = canonical_objects();
    let revision_id = ObjectId::of_canonical_bytes(&revision);
    let view_id = ObjectId::of_canonical_bytes(&view);

    assert_eq!(
        store.exists_many(&[revision_id, view_id]).await.unwrap(),
        vec![false, false]
    );
    store.put_if_absent(revision_id, &revision).await.unwrap();
    store.put_if_absent(revision_id, &revision).await.unwrap();
    assert_eq!(
        store.get(revision_id).await.unwrap().canonical_bytes,
        revision
    );
    assert_eq!(
        store.exists_many(&[revision_id, view_id]).await.unwrap(),
        vec![true, false]
    );

    assert!(matches!(
        store.put_if_absent(revision_id, &view).await,
        Err(ObjectStoreError::Corrupt { .. })
    ));
    assert!(matches!(
        store
            .put_if_absent(ObjectId::of_canonical_bytes(&[3, 1]), &[3, 1])
            .await,
        Err(ObjectStoreError::InvalidFormat(_))
    ));
}

#[tokio::test]
async fn apache_in_memory_backend_passes_immutable_conformance() {
    let backend: Arc<dyn BackendObjectStore> = Arc::new(InMemory::new());
    let store = ObjectStoreAdapter::new(backend, "test/objects");
    run_conformance(&store).await;
}

#[tokio::test]
#[ignore = "requires WHEAT_TEST_S3_* or scripts/wheat-seaweedfs-test"]
async fn configured_s3_backend_passes_immutable_conformance() {
    let Some(config) = s3_test_config() else {
        return;
    };
    let store = ObjectStoreAdapter::s3(&config).unwrap();
    run_conformance(&store).await;
}

#[tokio::test]
#[ignore = "requires WHEAT_TEST_S3_* or scripts/wheat-seaweedfs-test"]
async fn repository_closure_publishes_through_configured_s3_backend() {
    let Some(mut config) = s3_test_config() else {
        return;
    };
    config.prefix = format!("wheat-publication/{}", std::process::id());
    let objects = ObjectStoreAdapter::s3(&config).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let content = ContentStore::new(directory.path().join("content"));
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    content.put(b"value").unwrap();
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    repository
        .create_change(
            basis,
            basis,
            [],
            revision_edits("file", b"value"),
            ApplicationContext::new(&content),
        )
        .unwrap();

    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    assert_eq!(heads.read().unwrap(), head);
    let envelope = RepositoryReader::new(&objects, &content)
        .envelope(head.envelope)
        .await
        .unwrap();
    assert_eq!(
        objects
            .get(envelope.state.into())
            .await
            .unwrap()
            .canonical_bytes,
        repository.canonical_closure().state.canonical_bytes()
    );
}

fn s3_test_config() -> Option<S3ObjectStoreConfig> {
    let endpoint = std::env::var("WHEAT_TEST_S3_ENDPOINT").ok()?;
    Some(S3ObjectStoreConfig {
        endpoint,
        bucket: std::env::var("WHEAT_TEST_S3_BUCKET").unwrap(),
        region: std::env::var("WHEAT_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key_id: std::env::var("WHEAT_TEST_S3_ACCESS_KEY_ID").unwrap(),
        secret_access_key: std::env::var("WHEAT_TEST_S3_SECRET_ACCESS_KEY").unwrap(),
        prefix: format!("wheat-conformance/{}", std::process::id()),
        allow_http: true,
    })
}
