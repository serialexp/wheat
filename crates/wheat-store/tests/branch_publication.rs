use wheat_core::*;
use wheat_store::*;

#[tokio::test]
async fn named_branches_survive_reopen_and_ordinary_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let mut repo = Repository::new(Tree::new());
    let (_, outcome) = repo
        .create_change(
            [],
            RevisionEdits::default(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let branches = BranchManifestRecord::new([
        ("refs/heads/main", outcome.resulting_view),
        ("refs/heads/archive", repo.published_view().id),
    ])
    .unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher.prepare(&repo).await.unwrap();
    let envelope = publisher
        .prepare_envelope(closure.state.id, &branches)
        .await
        .unwrap();
    let initial = publisher
        .initialize_prepared_envelope(envelope)
        .await
        .unwrap();
    drop(repo);
    let reopened = LooseObjectStore::new(dir.path().join("objects"));
    let reader = RepositoryReader::new(&reopened, &content);
    let record = reader.envelope(initial.envelope).await.unwrap();
    assert_eq!(
        reader.branch_manifest(record.branches).await.unwrap(),
        branches
    );
    let mut editable = reader.hydrate(record.state).await.unwrap();
    editable
        .create_change(
            [],
            RevisionEdits::default(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let next = RepositoryPublisher::new(&reopened, &content, &heads)
        .publish(initial, &editable)
        .await
        .unwrap();
    let updated = reader.envelope(next.envelope).await.unwrap();
    assert_eq!(updated.branches, record.branches);
    assert_eq!(
        reader.branch_manifest(updated.branches).await.unwrap(),
        branches
    );
    assert_ne!(updated.state, record.state);
}

#[tokio::test]
async fn dangling_branch_target_never_creates_head() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher
        .prepare(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let branches =
        BranchManifestRecord::new([("refs/heads/missing", ViewId::from_digest([42; 32]))]).unwrap();
    let prepared = publisher
        .prepare_envelope(closure.state.id, &branches)
        .await;
    if let Ok(envelope) = prepared {
        assert!(publisher.initialize_envelope(envelope).await.is_err());
    }
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
}
