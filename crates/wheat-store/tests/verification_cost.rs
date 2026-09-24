use async_trait::async_trait;
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wheat_core::*;
use wheat_store::*;

struct Counting {
    inner: ObjectStoreAdapter,
    gets: AtomicUsize,
    changes: AtomicUsize,
    trees: AtomicUsize,
    content_set_blobs: AtomicUsize,
    revision_map_nodes: AtomicUsize,
    application_map_nodes: AtomicUsize,
    applications: AtomicUsize,
}
#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        let object = self.inner.get(id).await?;
        match object.kind {
            ObjectKind::Change => {
                self.changes.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::Tree => {
                self.trees.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::ContentSetSnapshot | ObjectKind::ContentSetDelta => {
                self.content_set_blobs.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::RevisionMapNode => {
                self.revision_map_nodes.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::ApplicationMapNode => {
                self.application_map_nodes.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::Application => {
                self.applications.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(object)
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[tokio::test]
async fn verification_indexes_changes_once_and_reuses_identical_tree_contexts() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path());
    let objects = Counting {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "verify-cost"),
        gets: AtomicUsize::new(0),
        changes: AtomicUsize::new(0),
        trees: AtomicUsize::new(0),
        content_set_blobs: AtomicUsize::new(0),
        revision_map_nodes: AtomicUsize::new(0),
        application_map_nodes: AtomicUsize::new(0),
        applications: AtomicUsize::new(0),
    };
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let mut effective = basis;
    let mut parent = None;
    for n in 1..=12u8 {
        let id = ChangeId::from_bytes([n; 16]);
        let outcome = repo
            .create_change_with_id(
                basis,
                effective,
                id,
                parent,
                RevisionEdits::default(),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
        parent = Some(id);
    }
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
    assert_eq!(
        objects.changes.load(Ordering::SeqCst),
        12,
        "each change record is indexed once, not scanned for every revision/dependency"
    );
    assert!(
        objects.trees.load(Ordering::SeqCst) < 4,
        "the same empty tree has only visible and aggregate-only verification contexts"
    );
}

#[tokio::test]
async fn cumulative_history_verification_is_bounded_by_shared_dag_size() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path());
    let objects = Counting {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "verify-history-cost"),
        gets: AtomicUsize::new(0),
        changes: AtomicUsize::new(0),
        trees: AtomicUsize::new(0),
        content_set_blobs: AtomicUsize::new(0),
        revision_map_nodes: AtomicUsize::new(0),
        application_map_nodes: AtomicUsize::new(0),
        applications: AtomicUsize::new(0),
    };
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let mut effective = basis;
    let mut parent = None;
    for n in 1..=64u8 {
        let id = ChangeId::from_bytes([n; 16]);
        let outcome = repo
            .create_change_with_id(
                basis,
                effective,
                id,
                parent,
                RevisionEdits::default(),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
        parent = Some(id);
    }
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    objects.gets.store(0, Ordering::SeqCst);
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
    let gets = objects.gets.load(Ordering::SeqCst);
    let content_set_blobs = objects.content_set_blobs.load(Ordering::SeqCst);
    let revision_map_nodes = objects.revision_map_nodes.load(Ordering::SeqCst);
    let application_map_nodes = objects.application_map_nodes.load(Ordering::SeqCst);
    let applications = objects.applications.load(Ordering::SeqCst);
    assert!(gets < 40_000, "shared history required {gets} object reads");
    assert!(
        content_set_blobs < 200,
        "contextual visible-set receipts reread {content_set_blobs} blobs"
    );
    assert!(
        revision_map_nodes < 200,
        "contextual revision-map receipts reread {revision_map_nodes} nodes"
    );
    assert!(
        application_map_nodes < 200,
        "contextual application/revision receipts reread {application_map_nodes} nodes"
    );
    assert!(
        applications < 200,
        "contextual application/revision receipts reread {applications} application records"
    );
}

#[tokio::test]
async fn each_verification_reauthenticates_content_instead_of_reusing_scratch_trust() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    let id = content.put(b"original").unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let repo = Repository::new(Tree::from([(
        "file".into(),
        wheat_core::FileValue {
            content: id,
            kind: wheat_core::FileKind::Regular,
        },
    )]));
    let closure = repo.canonical_closure();
    for object in closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_complete(closure.state.id).await.unwrap();
    std::fs::write(content.path(id), b"corrupted").unwrap();
    assert!(reader.validate_complete(closure.state.id).await.is_err());
}

#[tokio::test]
async fn duplicate_logical_changes_are_rejected_by_verified_index() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let (change, _) = repo
        .create_change(
            basis,
            basis,
            [],
            RevisionEdits::default(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let mut duplicate = repo.change(change).unwrap().clone();
    duplicate.metadata.title = "different object, same logical change".into();
    let bytes = duplicate.canonical_bytes();
    let id = ObjectId::of_canonical_bytes(&bytes);
    objects.put_if_absent(id, &bytes).await.unwrap();
    let mut state = decode_repository_state_record(&closure.state.canonical_bytes()).unwrap();
    state.retained_changes = RetainedSetStore::new(&objects)
        .insert(state.retained_changes, id)
        .await
        .unwrap();
    objects
        .put_if_absent(state.id().into(), &state.canonical_bytes())
        .await
        .unwrap();
    assert!(
        RepositoryReader::new(&objects, &content)
            .validate_complete(state.id())
            .await
            .is_err()
    );
}
