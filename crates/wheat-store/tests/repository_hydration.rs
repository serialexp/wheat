use wheat_core::*;
use wheat_store::*;

fn edit(expected: Option<&[u8]>, replacement: &[u8]) -> Transformation {
    Transformation {
        edits: vec![FileEdit {
            path: "file".into(),
            expected: expected.map(ContentId::of),
            replacement: Some(ContentId::of(replacement)),
        }],
    }
}

#[tokio::test]
async fn canonical_reopen_preserves_history_and_allows_revision_and_publication() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [b"one".as_slice(), b"two", b"three"] {
        content.put(bytes).unwrap();
    }
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let (head, change, revision, closure) = {
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let mut repo = Repository::new(Tree::new());
        let (change, first) = repo.create_change([], edit(None, b"one")).unwrap();
        let second = repo
            .revise_change(change, first.revision_id, edit(None, b"two"))
            .unwrap();
        let head = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repo)
            .await
            .unwrap();
        (head, change, second.revision_id, repo.canonical_closure())
    };
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let root = RepositoryStateId::from_digest(*head.repository_state.as_bytes());
    let mut reopened = RepositoryReader::new(&objects, &content)
        .hydrate(root)
        .await
        .unwrap();
    assert_eq!(reopened.canonical_closure(), closure);
    reopened
        .revise_change(change, revision, edit(None, b"three"))
        .unwrap();
    let next = RepositoryPublisher::new(&objects, &content, &heads)
        .publish(head, &reopened)
        .await
        .unwrap();
    let again = RepositoryReader::new(&objects, &content)
        .hydrate(RepositoryStateId::from_digest(
            *next.repository_state.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(again.working_view().tree["file"], ContentId::of(b"three"));
    assert_eq!(again.canonical_closure(), reopened.canonical_closure());
}

#[tokio::test]
async fn conflicted_stack_reopens_with_dependency_reapplication_intact() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in [b"base".as_slice(), b"a1", b"a2", b"b1"] {
        content.put(bytes).unwrap();
    }
    let (head, a, a2, expected) = {
        let mut repo = Repository::new(Tree::from([("file".into(), ContentId::of(b"base"))]));
        let (a, a1) = repo.create_change([], edit(Some(b"base"), b"a1")).unwrap();
        repo.create_change([a], edit(Some(b"a1"), b"b1")).unwrap();
        let a2 = repo
            .revise_change(a, a1.revision_id, edit(Some(b"base"), b"a2"))
            .unwrap();
        let head = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repo)
            .await
            .unwrap();
        (head, a, a2.revision_id, repo.canonical_closure())
    };
    let mut repo = RepositoryReader::new(&objects, &content)
        .hydrate(RepositoryStateId::from_digest(
            *head.repository_state.as_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(repo.canonical_closure(), expected);
    assert!(!repo.working_view().unresolved_conflicts.is_empty());
    // Restore the parent's old contents; the reconstructed dependency graph must reapply B.
    repo.revise_change(a, a2, edit(Some(b"base"), b"a1"))
        .unwrap();
    assert!(repo.working_view().unresolved_conflicts.is_empty());
    assert_eq!(repo.working_view().tree["file"], ContentId::of(b"b1"));
    RepositoryPublisher::new(&objects, &content, &heads)
        .publish(head, &repo)
        .await
        .unwrap();
}

#[tokio::test]
async fn reopen_rejects_missing_historical_content_before_returning_editable_state() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let historical = content.put(b"old").unwrap();
    content.put(b"new").unwrap();
    let mut repo = Repository::new(Tree::new());
    let (change, first) = repo.create_change([], edit(None, b"old")).unwrap();
    repo.revise_change(change, first.revision_id, edit(None, b"new"))
        .unwrap();
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    drop(repo);
    std::fs::remove_file(content.path(historical)).unwrap();
    assert!(
        RepositoryReader::new(&objects, &content)
            .hydrate(RepositoryStateId::from_digest(
                *head.repository_state.as_bytes()
            ))
            .await
            .is_err()
    );
    assert_eq!(heads.read().unwrap(), head);
}
