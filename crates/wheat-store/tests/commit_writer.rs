use wheat_core::*;
use wheat_store::*;

fn proposal(n: u8, parent: Option<ChangeId>, edits: Vec<FileEdit>) -> ChangeProposal {
    ChangeProposal {
        id: ChangeId::from_bytes([n; 16]),
        metadata: ChangeMetadata::default(),
        dependencies: parent.into_iter().collect(),
        revision_metadata: RevisionMetadata::default(),
        transformation: Transformation { edits },
    }
}
fn edit(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> FileEdit {
    FileEdit {
        path: path.into(),
        expected: old.map(ContentId::of),
        replacement: new.map(ContentId::of),
    }
}
#[tokio::test]
async fn persisted_commit_ids_match_reference_for_branches_deletions_and_noops() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    content.put(b"shared").unwrap();
    content.put(b"other").unwrap();
    let writer = PersistedCommitWriter::new(&objects, &content);
    let empty = writer.empty_view().await.unwrap();
    let a = proposal(
        1,
        None,
        vec![
            edit("a", None, Some(b"shared")),
            edit("b", None, Some(b"shared")),
        ],
    );
    let b = proposal(2, Some(a.id), vec![edit("a", Some(b"shared"), None)]);
    let c = proposal(
        3,
        Some(a.id),
        vec![edit("a", Some(b"shared"), Some(b"other"))],
    );
    let d = proposal(4, Some(b.id), vec![]);
    let mut reference = Repository::new(Tree::new());
    let expected = reference
        .import_changes([a.clone(), b.clone(), c.clone(), d.clone()])
        .unwrap();
    let mut roots = std::collections::BTreeMap::new();
    for p in [a, b, c, d] {
        let parent = p.dependencies.first().map(|id| roots[id]).unwrap_or(empty);
        let id = p.id;
        let result = writer.write(parent, p).await.unwrap();
        assert_eq!(result.revision_id, expected[&id].revision_id);
        assert_eq!(result.resulting_view, expected[&id].resulting_view);
        let view = reference.view(result.resulting_view).unwrap();
        assert_eq!(result.application_id, view.applications[&id]);
        roots.insert(id, result.resulting_view);
    }
    let reopened = LooseObjectStore::new(dir.path().join("objects"));
    let reader = RepositoryReader::new(&reopened, &content);
    let view = reader
        .view(roots[&ChangeId::from_bytes([2; 16])])
        .await
        .unwrap();
    let manifest = reader.manifest(view.required_content).await.unwrap();
    assert!(
        ContentSetStore::new(&reopened)
            .contains(manifest.content, ContentId::of(b"shared"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn file_directory_replacements_and_invalid_expected_preserve_old_root() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [b"one".as_slice(), b"two", b"wrong"] {
        content.put(bytes).unwrap();
    }
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let a = writer
        .write(base, proposal(1, None, vec![edit("a", None, Some(b"one"))]))
        .await
        .unwrap();
    let b = writer
        .write(
            a.resulting_view,
            proposal(
                2,
                Some(a.change_id),
                vec![
                    edit("a", Some(b"one"), None),
                    edit("a/b", None, Some(b"two")),
                ],
            ),
        )
        .await
        .unwrap();
    let c = writer
        .write(
            b.resulting_view,
            proposal(
                3,
                Some(b.change_id),
                vec![
                    edit("a", None, Some(b"one")),
                    edit("a/b", Some(b"two"), None),
                ],
            ),
        )
        .await
        .unwrap();
    assert!(
        writer
            .write(
                c.resulting_view,
                proposal(4, Some(c.change_id), vec![edit("a", Some(b"wrong"), None)])
            )
            .await
            .is_err()
    );
    let reader = RepositoryReader::new(&objects, &content);
    let tree = reader.view(a.resulting_view).await.unwrap().tree;
    assert_eq!(
        MerkleTreeStore::new(&objects)
            .lookup(tree, &["a".into()])
            .await
            .unwrap(),
        Some(ContentId::of(b"one"))
    );
}

#[tokio::test]
async fn inserting_file_over_untouched_directory_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    content.put(b"one").unwrap();
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let a = writer
        .write(
            base,
            proposal(1, None, vec![edit("a/b", None, Some(b"one"))]),
        )
        .await
        .unwrap();
    assert!(
        writer
            .write(
                a.resulting_view,
                proposal(2, Some(a.change_id), vec![edit("a", None, Some(b"one"))])
            )
            .await
            .is_err()
    );
}
