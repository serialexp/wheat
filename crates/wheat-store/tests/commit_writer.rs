use std::collections::BTreeMap;
use wheat_core::*;
use wheat_store::*;

fn value(bytes: &[u8]) -> FileValue {
    FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    }
}
fn proposal(n: u8, parent: Option<ChangeId>, records: Vec<EditRecord>) -> ChangeProposal {
    let edits = records
        .into_iter()
        .map(|record| (record.id(), record))
        .collect::<BTreeMap<_, _>>();
    ChangeProposal {
        id: ChangeId::from_bytes([n; 16]),
        metadata: Default::default(),
        dependencies: parent.into_iter().collect(),
        revision_metadata: Default::default(),
        edits: RevisionEdits {
            transition: EditTransition {
                added: edits.keys().copied().collect(),
                ..Default::default()
            },
            edits,
        },
    }
}
fn exact(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> EditRecord {
    EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: old.map(value),
            replacement: new.map(value),
        },
    )
    .unwrap()
}
fn guarded(
    path: &str,
    before: &[u8],
    expected: &[u8],
    replacement: &[u8],
    after: &[u8],
) -> EditRecord {
    EditRecord::new(
        path,
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(before),
            expected: ContentId::of(expected),
            replacement: ContentId::of(replacement),
            after: ContentId::of(after),
            start_anchor: before.is_empty(),
            end_anchor: after.is_empty(),
        },
    )
    .unwrap()
}
fn retain(content: &ContentStore, values: &[&[u8]]) {
    for value in values {
        content.put(value).unwrap();
    }
}

#[tokio::test]
async fn persisted_clean_commit_stores_definitions_and_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    retain(&content, &[b"one", b"two"]);
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let record = exact("a", None, Some(b"one"));
    let edit_id = record.id();
    let authored = proposal(1, None, vec![record.clone()]);
    let mut reference = Repository::new(Tree::new());
    let reference_base = reference.base_view().id;
    let expected = reference
        .import_changes(
            reference_base,
            reference_base,
            [authored.clone()],
            ApplicationContext::new(&content),
        )
        .unwrap()[&authored.id]
        .clone();
    let commit = writer.write(base, authored.clone()).await.unwrap();
    assert_eq!(commit.revision_id, expected.revision_id);
    assert_eq!(commit.resulting_view, expected.resulting_view);
    assert_eq!(
        commit.application_id,
        reference
            .view(expected.resulting_view)
            .unwrap()
            .applications[&authored.id]
    );
    let reader = RepositoryReader::new(&objects, &content);
    assert_eq!(reader.edit(edit_id).await.unwrap(), record);
    let application = reader.application(commit.application_id).await.unwrap();
    assert_eq!(application.applied[&edit_id].target, None);
    assert_eq!(
        MerkleTreeStore::new(&objects)
            .lookup(
                reader.view(commit.resulting_view).await.unwrap().tree,
                &["a".into()]
            )
            .await
            .unwrap(),
        Some(value(b"one"))
    );
}

#[tokio::test]
async fn multiple_guarded_edits_on_one_path_apply_and_record_original_target() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    retain(
        &content,
        &[
            b"alpha beta",
            b"",
            b"alpha",
            b"ALPHA",
            b" ",
            b"beta",
            b"BETA",
        ],
    );
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let seed = writer
        .write(
            base,
            proposal(1, None, vec![exact("f", None, Some(b"alpha beta"))]),
        )
        .await
        .unwrap();
    let first = guarded("f", b"", b"alpha", b"ALPHA", b" ");
    let second = guarded("f", b" ", b"beta", b"BETA", b"");
    let ids = [first.id(), second.id()];
    let result = writer
        .write(
            seed.resulting_view,
            proposal(2, Some(seed.change_id), vec![first, second]),
        )
        .await
        .unwrap();
    assert_eq!(
        content.get(ContentId::of(b"ALPHA BETA")).unwrap(),
        b"ALPHA BETA"
    );
    let reader = RepositoryReader::new(&objects, &content);
    let application = reader.application(result.application_id).await.unwrap();
    let view = reader.view(result.resulting_view).await.unwrap();
    assert_eq!(
        ContentMapStore::new(&objects)
            .get(view.required_content, ContentId::of(b"ALPHA BETA"))
            .await
            .unwrap(),
        Some(1)
    );
    for bytes in [
        b"alpha beta".as_slice(),
        b"",
        b"alpha",
        b"ALPHA",
        b" ",
        b"beta",
        b"BETA",
    ] {
        assert_eq!(
            ContentMapStore::new(&objects)
                .get(view.required_content, ContentId::of(bytes))
                .await
                .unwrap(),
            None,
            "view content map retained historical fragment {:?}",
            String::from_utf8_lossy(bytes)
        );
    }
    for id in ids {
        assert_eq!(application.applied[&id].target, Some(value(b"alpha beta")));
        assert!(application.applied[&id].read.is_some());
    }
}

#[tokio::test]
async fn failed_guard_and_budget_publish_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    retain(&content, &[b"text", b"", b"missing", b"new"]);
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let seed = writer
        .write(
            base,
            proposal(1, None, vec![exact("f", None, Some(b"text"))]),
        )
        .await
        .unwrap();
    let bad = proposal(
        2,
        Some(seed.change_id),
        vec![guarded("f", b"", b"missing", b"new", b"")],
    );
    let id = bad.id;
    assert!(matches!(
        writer.write(seed.resulting_view, bad).await.unwrap_err(),
        CommitWriterError::FailedEdits(_)
    ));
    assert!(
        RevisionMapStore::new(&objects)
            .get(
                RepositoryReader::new(&objects, &content)
                    .view(seed.resulting_view)
                    .await
                    .unwrap()
                    .revisions,
                id
            )
            .await
            .unwrap()
            .is_none()
    );
    let limits = edit_application::ApplicationLimits {
        max_edits: 0,
        ..Default::default()
    };
    let limited = PersistedCommitWriter::with_limits(&objects, &content, limits);
    assert!(matches!(
        limited
            .write(
                seed.resulting_view,
                proposal(
                    3,
                    Some(seed.change_id),
                    vec![exact("x", None, Some(b"new"))]
                )
            )
            .await
            .unwrap_err(),
        CommitWriterError::Application(edit_application::ApplicationError::TooManyEdits { .. })
    ));
}

#[tokio::test]
async fn shape_changes_work_but_file_over_untouched_directory_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    retain(&content, &[b"one", b"two"]);
    let writer = PersistedCommitWriter::new(&objects, &content);
    let base = writer.empty_view().await.unwrap();
    let a = writer
        .write(
            base,
            proposal(1, None, vec![exact("a", None, Some(b"one"))]),
        )
        .await
        .unwrap();
    let b = writer
        .write(
            a.resulting_view,
            proposal(
                2,
                Some(a.change_id),
                vec![
                    exact("a", Some(b"one"), None),
                    exact("a/b", None, Some(b"two")),
                ],
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        writer
            .write(
                b.resulting_view,
                proposal(3, Some(b.change_id), vec![exact("a", None, Some(b"one"))])
            )
            .await
            .unwrap_err(),
        CommitWriterError::InvalidPath(_)
    ));
}
