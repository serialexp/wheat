use std::collections::{BTreeMap, BTreeSet};

use wheat_core::*;
use wheat_store::*;

fn exact(expected: Option<&[u8]>, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
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
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

async fn fixture(
    root: &std::path::Path,
) -> (
    LooseObjectStore,
    ContentStore,
    RepositoryClosure,
    RevisionRecord,
    ApplicationRecord,
    EditId,
) {
    let objects = LooseObjectStore::new(root.join("objects"));
    let content = ContentStore::new(root.join("content"));
    for bytes in [b"before".as_slice(), b"after"] {
        content.put(bytes).unwrap();
    }
    let mut repository = Repository::new(Tree::from([(
        "file".into(),
        FileValue {
            content: ContentId::of(b"before"),
            kind: FileKind::Regular,
        },
    )]));
    repository
        .create_change(
            [],
            exact(Some(b"before"), b"after"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let closure = RepositoryPublisher::new(
        &objects,
        &content,
        &LocalPublicationStore::new(root.join("heads")),
    )
    .prepare(&repository)
    .await
    .unwrap();
    let revision = closure
        .objects
        .iter()
        .find_map(|object| decode_revision_record(&object.bytes).ok())
        .unwrap();
    let application = closure
        .objects
        .iter()
        .find_map(|object| decode_application_record(&object.bytes).ok())
        .unwrap();
    let edit = *revision.edits.iter().next().unwrap();
    (objects, content, closure, revision, application, edit)
}

async fn extend_state(
    objects: &LooseObjectStore,
    state: &RepositoryState,
    revisions: BTreeSet<RevisionId>,
    applications: BTreeSet<ApplicationId>,
    extra: impl IntoIterator<Item = CanonicalObject>,
) -> RepositoryStateId {
    for object in extra {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let mut emitted = Vec::new();
    let replacement = RepositoryState {
        id: RepositoryStateId::from_digest([0; 32]),
        base_view: state.base_view,
        working_view: state.working_view,
        published_view: state.published_view,
        change_objects: state.change_objects.clone(),
        required_content: state.required_content,
        retained_changes: state.retained_changes.clone(),
        retained_views: state.retained_views.clone(),
        retained_revisions: revisions,
        retained_applications: applications,
    };
    let record = build_repository_state_record(&replacement, &mut |object| emitted.push(object));
    for object in emitted {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    objects
        .put_if_absent(record.id().into(), &record.canonical_bytes())
        .await
        .unwrap();
    record.id()
}

#[tokio::test]
async fn rejects_corrupt_retained_historical_edit_definition() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, closure, _, _, edit) = fixture(dir.path()).await;
    std::fs::write(objects.path(edit.into()), b"corrupt historical edit").unwrap();
    assert!(
        RepositoryReader::new(&objects, &content)
            .validate_complete(closure.state.id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rejects_self_hashed_fabricated_revision_result() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, closure, mut revision, _, _) = fixture(dir.path()).await;
    let base = RepositoryReader::new(&objects, &content)
        .view(revision.authored_against)
        .await
        .unwrap();
    revision.resulting_tree = base.tree;
    let fabricated = revision.id();
    let mut revisions = closure.state.retained_revisions.clone();
    revisions.insert(fabricated);
    let root = extend_state(
        &objects,
        &closure.state,
        revisions,
        closure.state.retained_applications.clone(),
        [CanonicalObject {
            id: fabricated.into(),
            bytes: revision.canonical_bytes(),
        }],
    )
    .await;
    assert!(
        RepositoryReader::new(&objects, &content)
            .validate_complete(root)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rejects_self_hashed_fabricated_application_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, closure, _, mut application, _) = fixture(dir.path()).await;
    application.applied.clear();
    let fabricated = application.id();
    let mut applications = closure.state.retained_applications.clone();
    applications.insert(fabricated);
    let root = extend_state(
        &objects,
        &closure.state,
        closure.state.retained_revisions.clone(),
        applications,
        [CanonicalObject {
            id: fabricated.into(),
            bytes: application.canonical_bytes(),
        }],
    )
    .await;
    assert!(
        RepositoryReader::new(&objects, &content)
            .validate_complete(root)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn guarded_repository_verifies_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, closure, _, _, _) = fixture(dir.path()).await;
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_complete(closure.state.id).await.unwrap();
    let reopened = reader.hydrate(closure.state.id).await.unwrap();
    assert_eq!(reopened.canonical_closure(), closure);
}

fn exact_at(path: &str, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> EditRecord {
    EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
            replacement: replacement.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap()
}

fn edits(records: Vec<EditRecord>) -> RevisionEdits {
    let edits: BTreeMap<_, _> = records
        .into_iter()
        .map(|record| (record.id(), record))
        .collect();
    RevisionEdits {
        transition: EditTransition {
            added: edits.keys().copied().collect(),
            ..EditTransition::default()
        },
        edits,
    }
}

#[tokio::test]
async fn verifier_accepts_empty_to_nested_exact_edit() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    content.put(b"nested").unwrap();
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change(
            [],
            edits(vec![exact_at("empty/nested", None, Some(b"nested"))]),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let closure = RepositoryPublisher::new(
        &objects,
        &content,
        &LocalPublicationStore::new(dir.path().join("heads")),
    )
    .prepare(&repository)
    .await
    .unwrap();
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn verifier_accepts_nested_directory_to_missing_exact_edit() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    content.put(b"nested").unwrap();
    let mut repository = Repository::new(Tree::from([(
        "empty/nested".into(),
        FileValue {
            content: ContentId::of(b"nested"),
            kind: FileKind::Regular,
        },
    )]));
    repository
        .create_change(
            [],
            edits(vec![exact_at("empty/nested", Some(b"nested"), None)]),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let closure = RepositoryPublisher::new(
        &objects,
        &content,
        &LocalPublicationStore::new(dir.path().join("heads")),
    )
    .prepare(&repository)
    .await
    .unwrap();
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
}
