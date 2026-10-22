use wheat_core::*;
use wheat_store::*;

#[tokio::test]
async fn named_labels_survive_reopen_and_ordinary_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let (_, outcome) = repo
        .create_change(
            basis,
            basis,
            [],
            RevisionEdits::default(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let labels =
        ViewLabelManifestRecord::new([("main", outcome.resulting_view), ("archive", basis)])
            .unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher.prepare(&repo).await.unwrap();
    let envelope = publisher
        .prepare_envelope(closure.state.id, &labels)
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
        reader.view_label_manifest(record.labels).await.unwrap(),
        labels
    );
    let mut editable = reader.hydrate(record.state).await.unwrap();
    let (new_change, new_outcome) = editable
        .create_change(
            basis,
            outcome.resulting_view,
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
    let updated_repository = reader.hydrate(updated.state).await.unwrap();
    assert_eq!(
        updated_repository.current_revision(new_outcome.resulting_view, new_change),
        Some(new_outcome.revision_id),
        "the explicitly selected effective view must survive publication and reopen"
    );
    assert_eq!(updated.labels, record.labels);
    assert_eq!(
        reader.view_label_manifest(updated.labels).await.unwrap(),
        labels
    );
    assert_ne!(updated.state, record.state);
}

#[tokio::test]
async fn dangling_label_target_never_creates_head() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher
        .prepare(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let labels =
        ViewLabelManifestRecord::new([("missing", ViewId::from_digest([42; 32]))]).unwrap();
    let prepared = publisher.prepare_envelope(closure.state.id, &labels).await;
    if let Ok(envelope) = prepared {
        assert!(publisher.initialize_envelope(envelope).await.is_err());
    }
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
}

/// A label publish checks only what the new manifest changes against the head it
/// replaces, which still refuses a label naming a view the state doesn't retain.
#[tokio::test]
async fn a_staged_label_naming_an_unretained_view_is_not_published() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let initial = publisher.initialize(&repo).await.unwrap();
    let state = RepositoryReader::new(&objects, &content)
        .envelope(initial.envelope)
        .await
        .unwrap()
        .state;

    let dangling =
        ViewLabelManifestRecord::new([("missing", ViewId::from_digest([42; 32]))]).unwrap();
    let staged = publisher.stage_envelope(state, &dangling).await.unwrap();
    assert!(publisher.publish_envelope(initial, staged).await.is_err());
    assert_eq!(heads.read().unwrap(), initial);

    let labels = ViewLabelManifestRecord::new([("main", basis)]).unwrap();
    let staged = publisher.stage_envelope(state, &labels).await.unwrap();
    let published = publisher.publish_envelope(initial, staged).await.unwrap();
    assert_eq!(published.envelope, staged);
}
