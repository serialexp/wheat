use wheat_core::*;
use wheat_store::*;

fn edit(
    expected: Option<&[u8]>,
    replacement: &[u8],
    removed: impl IntoIterator<Item = EditId>,
) -> RevisionEdits {
    let definition = EditRecord::new(
        "file",
        EditOperation::ExactFile {
            expected: expected.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
            replacement: Some(FileValue {
                content: ContentId::of(replacement),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = definition.id();
    RevisionEdits {
        edits: [(id, definition)].into(),
        transition: EditTransition {
            added: [id].into(),
            removed: removed.into_iter().collect(),
            ..EditTransition::default()
        },
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
    let (head, change, revision, mut effective, closure) = {
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let mut repo = Repository::new(Tree::new());
        let base = repo.base_view().id;
        let mut effective = base;
        let context = ApplicationContext::new(&content);
        let (change, first) = repo
            .create_change(base, effective, [], edit(None, b"one", []), context)
            .unwrap();
        effective = first.resulting_view;
        let removed = repo
            .revision(first.revision_id)
            .unwrap()
            .edits()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let second = repo
            .revise_change(
                base,
                effective,
                change,
                first.revision_id,
                edit(None, b"two", removed),
                context,
            )
            .unwrap();
        effective = second.resulting_view;
        let head = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repo)
            .await
            .unwrap();
        (
            head,
            change,
            second.revision_id,
            effective,
            repo.canonical_closure(),
        )
    };
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let reader = RepositoryReader::new(&objects, &content);
    let initial_envelope = reader.envelope(head.envelope).await.unwrap();
    let mut reopened = reader.hydrate(initial_envelope.state).await.unwrap();
    assert_eq!(reopened.canonical_closure(), closure);
    let removed = reopened
        .revision(revision)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let third = reopened
        .revise_change(
            reopened.base_view().id,
            effective,
            change,
            revision,
            edit(None, b"three", removed),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = third.resulting_view;
    let next = RepositoryPublisher::new(&objects, &content, &heads)
        .publish(head, &reopened)
        .await
        .unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let root = reader.envelope(next.envelope).await.unwrap().state;
    let again = reader.hydrate(root).await.unwrap();
    assert_eq!(
        again.view(effective).unwrap().tree["file"].content,
        ContentId::of(b"three")
    );
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
    let (head, a, a2, base, effective, expected) = {
        let mut repo = Repository::new(Tree::from([(
            "file".into(),
            wheat_core::FileValue {
                content: ContentId::of(b"base"),
                kind: wheat_core::FileKind::Regular,
            },
        )]));
        let base = repo.base_view().id;
        let mut effective = base;
        let context = ApplicationContext::new(&content);
        let (a, a1) = repo
            .create_change(base, effective, [], edit(Some(b"base"), b"a1", []), context)
            .unwrap();
        effective = a1.resulting_view;
        let (_, b1) = repo
            .create_change(base, effective, [a], edit(Some(b"a1"), b"b1", []), context)
            .unwrap();
        effective = b1.resulting_view;
        let removed = repo
            .revision(a1.revision_id)
            .unwrap()
            .edits()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let a2 = repo
            .revise_change(
                base,
                effective,
                a,
                a1.revision_id,
                edit(Some(b"base"), b"a2", removed),
                context,
            )
            .unwrap();
        effective = a2.resulting_view;
        let head = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repo)
            .await
            .unwrap();
        (
            head,
            a,
            a2.revision_id,
            base,
            effective,
            repo.canonical_closure(),
        )
    };
    let reader = RepositoryReader::new(&objects, &content);
    let root = reader.envelope(head.envelope).await.unwrap().state;
    let mut repo = reader.hydrate(root).await.unwrap();
    assert_eq!(repo.canonical_closure(), expected);
    assert!(
        !repo
            .view(effective)
            .unwrap()
            .unresolved_conflicts
            .is_empty()
    );
    // Restore the parent's old contents; the reconstructed dependency graph must reapply B.
    let removed = repo
        .revision(a2)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let restored = repo
        .revise_change(
            base,
            effective,
            a,
            a2,
            edit(Some(b"base"), b"a1", removed),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let effective = restored.resulting_view;
    assert!(
        repo.view(effective)
            .unwrap()
            .unresolved_conflicts
            .is_empty()
    );
    assert_eq!(
        repo.view(effective).unwrap().tree["file"].content,
        ContentId::of(b"b1")
    );
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
    let base = repo.base_view().id;
    let mut effective = base;
    let context = ApplicationContext::new(&content);
    let (change, first) = repo
        .create_change(base, effective, [], edit(None, b"old", []), context)
        .unwrap();
    effective = first.resulting_view;
    let removed = repo
        .revision(first.revision_id)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    repo.revise_change(
        base,
        effective,
        change,
        first.revision_id,
        edit(None, b"new", removed),
        context,
    )
    .unwrap();
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    drop(repo);
    std::fs::remove_file(content.path(historical)).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let root = reader.envelope(head.envelope).await.unwrap().state;
    assert!(reader.hydrate(root).await.is_err());
    assert_eq!(heads.read().unwrap(), head);
}
