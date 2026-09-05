use async_trait::async_trait;
use std::fs;
use wheat_core::{ContentId, FileEdit, ObjectId, Repository, Transformation, Tree};
use wheat_store::*;

fn edit(value: &[u8]) -> Transformation {
    Transformation {
        edits: vec![FileEdit {
            path: "file".into(),
            expected: None,
            replacement: Some(ContentId::of(value)),
        }],
    }
}

struct ClaimsPresent(LooseObjectStore);
#[async_trait]
impl ImmutableObjectStore for ClaimsPresent {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.0.put_if_absent(id, bytes).await
    }
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.0.get(id).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        Ok(vec![true; ids.len()])
    }
}

#[tokio::test]
async fn root_publication_rejects_missing_descendant_even_when_head_requests_claim_presence() {
    let dir = tempfile::tempdir().unwrap();
    let objects = ClaimsPresent(LooseObjectStore::new(dir.path().join("objects")));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    content.put(b"hello").unwrap();
    let mut repo = Repository::new(Tree::new());
    repo.create_change([], edit(b"hello")).unwrap();
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let victim = closure
        .objects
        .iter()
        .find(|object| {
            wheat_core::validate_canonical_object(&object.bytes)
                == Ok(wheat_core::CanonicalObjectKind::Tree)
        })
        .unwrap();
    fs::remove_file(objects.0.path(victim.id)).unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    assert!(publisher.initialize_root(closure.state.id).await.is_err());
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
    objects
        .put_if_absent(victim.id, &victim.bytes)
        .await
        .unwrap();
    assert!(publisher.initialize_root(closure.state.id).await.is_ok());
}

#[tokio::test]
async fn reopen_and_publish_only_persisted_roots_without_repository() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    content.put(b"first").unwrap();
    content.put(b"second").unwrap();
    let (first, second) = {
        let mut repo = Repository::new(Tree::new());
        let (change, outcome) = repo.create_change([], edit(b"first")).unwrap();
        let publisher = RepositoryPublisher::new(&objects, &content, &heads);
        let first = publisher.prepare(&repo).await.unwrap().state.id;
        repo.revise_change(change, outcome.revision_id, edit(b"second"))
            .unwrap();
        let second = publisher.prepare(&repo).await.unwrap().state.id;
        (first, second)
    };
    let reopened = LooseObjectStore::new(dir.path().join("objects"));
    let publisher = RepositoryPublisher::new(&reopened, &content, &heads);
    let initial = publisher.initialize_root(first).await.unwrap();
    let next = publisher.publish_root(initial, second).await.unwrap();
    assert_eq!(next.repository_state, ObjectId::from(second));
    assert!(matches!(
        publisher.publish_root(initial, first).await,
        Err(RepositoryPublicationError::Head(
            PublicationError::Stale { .. }
        ))
    ));
    assert_eq!(heads.read().unwrap(), next);
}

#[tokio::test]
async fn conflicted_history_retains_conflict_terms_and_rejects_incomplete_aggregate() {
    use wheat_core::{ContentManifestRecord, build_content_set, decode_repository_state_record};
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in [b"base".as_slice(), b"a1", b"b1", b"a2"] {
        content.put(bytes).unwrap();
    }
    let transform = |expected: &[u8], replacement: &[u8]| Transformation {
        edits: vec![FileEdit {
            path: "file".into(),
            expected: Some(ContentId::of(expected)),
            replacement: Some(ContentId::of(replacement)),
        }],
    };
    let mut repo = Repository::new(Tree::from([("file".into(), ContentId::of(b"base"))]));
    let (a, first) = repo.create_change([], transform(b"base", b"a1")).unwrap();
    repo.create_change([a], transform(b"a1", b"b1")).unwrap();
    repo.revise_change(a, first.revision_id, transform(b"base", b"a2"))
        .unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher.prepare(&repo).await.unwrap();
    drop(repo);
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_complete(closure.state.id).await.unwrap();
    let initial = publisher.initialize_root(closure.state.id).await.unwrap();
    let mut state = decode_repository_state_record(&closure.state.canonical_bytes()).unwrap();
    let mut emitted = Vec::new();
    let incomplete = build_content_set([ContentId::of(b"base"), ContentId::of(b"a2")], |o| {
        emitted.push(o)
    });
    for object in emitted {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let manifest = ContentManifestRecord {
        content: incomplete,
    };
    objects
        .put_if_absent(manifest.id().into(), &manifest.canonical_bytes())
        .await
        .unwrap();
    state.required_content = manifest.id();
    objects
        .put_if_absent(state.id().into(), &state.canonical_bytes())
        .await
        .unwrap();
    assert!(publisher.publish_root(initial, state.id()).await.is_err());
    assert_eq!(heads.read().unwrap(), initial);
}

#[tokio::test]
async fn ordinary_publication_cannot_discard_history() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    content.put(b"retained").unwrap();
    let mut repo = Repository::new(Tree::new());
    repo.create_change([], edit(b"retained")).unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial = publisher.initialize(&repo).await.unwrap();
    let unrelated = Repository::new(Tree::new());
    let root = publisher.prepare(&unrelated).await.unwrap().state.id;
    assert!(publisher.publish_root(initial, root).await.is_err());
    assert!(publisher.publish(initial, &unrelated).await.is_err());
    assert_eq!(heads.read().unwrap(), initial);
}
