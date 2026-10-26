//! Diffing a served repository, which has no workspace.

use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use wheat_client::repository_diff::{self, DiffSource, TreeSideInput};
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition,
    FileKind, FileValue, RevisionEdits, RevisionMetadata,
};
use wheat_store::{
    PersistedRepositoryMutator, PublicationStore, RepositoryBrowser, RepositoryDirectory,
    TreePathFilter,
};

fn add(path: &str, bytes: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(bytes),
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
            ..EditTransition::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_served_repository_diffs_two_views() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));
    let head = directory.create_empty("main").await.unwrap();
    let objects = directory.open_store().unwrap();
    let content = objects.content_store();
    let heads = directory.publications();
    let main = RepositoryBrowser::open(&objects, &content, head)
        .await
        .unwrap()
        .label("main")
        .unwrap();

    content.put(b"hello\n").unwrap();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, outcome) = mutator
        .create_on_frontier(
            head,
            main,
            main,
            ChangeProposal {
                id: ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
                metadata: ChangeMetadata::default(),
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata::default(),
                edits: add("docs/readme", b"hello\n"),
            },
        )
        .await
        .unwrap();
    mutator.publish(prepared).await.unwrap();

    let source = DiffSource {
        objects: &objects,
        content: &content,
        head: heads.read().unwrap(),
    };
    let diff = repository_diff::views(
        source,
        "label:main",
        &outcome.resulting_view.to_string(),
        &TreePathFilter::default(),
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap();

    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "docs/readme");
    assert_eq!(diff.changes[0].old, None);
    assert_eq!(
        diff.changes[0].new,
        Some(TreeSideInput {
            value: FileValue {
                content: ContentId::of(b"hello\n"),
                kind: FileKind::Regular,
            },
            bytes: Some(b"hello\n".to_vec()),
            min_bytes: 6,
        })
    );
}
