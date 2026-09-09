use std::{collections::BTreeMap, path::Path, process::Command};

use gix::bstr::ByteSlice;
use wheat_core::{
    Actor, ChangeId, ChangeProposal, Conflict, EditOperation, EditRecord, EditTransition, FileKind,
    FileValue, RevisionEdits, RevisionMetadata, SourceProvenance, Timestamp, ViewRecord,
};
use wheat_git::export::{ExportError, ExportOptions, export_revision_history};
use wheat_store::{
    ConflictSetStore, ContentStore, ImmutableObjectStore, LooseObjectStore, PersistedCommit,
    PersistedCommitWriter, RepositoryReader,
};

struct Fixture {
    _directory: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        Self {
            objects: LooseObjectStore::new(directory.path().join("objects")),
            content: ContentStore::new(directory.path().join("content")),
            _directory: directory,
        }
    }
}

fn value(content: &ContentStore, bytes: &[u8], kind: FileKind) -> FileValue {
    FileValue {
        content: content.put(bytes).unwrap(),
        kind,
    }
}

fn proposal(
    number: u8,
    parent: Option<ChangeId>,
    path: &str,
    expected: Option<FileValue>,
    replacement: Option<FileValue>,
    metadata: RevisionMetadata,
) -> ChangeProposal {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected,
            replacement,
        },
    )
    .unwrap();
    let edit_id = edit.id();
    ChangeProposal {
        id: ChangeId::from_bytes([number; 16]),
        metadata: Default::default(),
        dependencies: parent.into_iter().collect(),
        revision_metadata: metadata,
        edits: RevisionEdits {
            edits: BTreeMap::from([(edit_id, edit)]),
            transition: EditTransition {
                added: [edit_id].into_iter().collect(),
                ..Default::default()
            },
        },
    }
}

fn metadata(message: &str, seconds: i64) -> RevisionMetadata {
    RevisionMetadata {
        message: message.into(),
        author: Actor {
            name: "Alice Author".into(),
            email: "alice@example.com".into(),
        },
        authored_at: Timestamp {
            seconds,
            offset_minutes: 120,
        },
        committer: Actor {
            name: "Casey Committer".into(),
            email: "casey@example.com".into(),
        },
        committed_at: Timestamp {
            seconds: seconds + 60,
            offset_minutes: -180,
        },
        provenance: None,
    }
}

fn bare() -> (tempfile::TempDir, gix::Repository) {
    let directory = tempfile::tempdir().unwrap();
    let repository = gix::init_bare(directory.path()).unwrap();
    (directory, repository)
}

fn git(repo: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

async fn two_revisions(
    fixture: &Fixture,
) -> (wheat_core::ViewId, PersistedCommit, PersistedCommit) {
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let first_value = value(&fixture.content, b"one\n", FileKind::Regular);
    let first = writer
        .write(
            base,
            proposal(
                1,
                None,
                "file.txt",
                None,
                Some(first_value),
                metadata("first revision\n", 1_700_000_000),
            ),
        )
        .await
        .unwrap();
    let second = writer
        .write(
            first.resulting_view,
            proposal(
                2,
                Some(first.change_id),
                "file.txt",
                Some(first_value),
                Some(value(&fixture.content, b"two\n", FileKind::Regular)),
                metadata("second revision\n", 1_700_000_100),
            ),
        )
        .await
        .unwrap();
    (base, first, second)
}

#[tokio::test]
async fn projects_multiple_native_revisions_deterministically_and_passes_fsck() {
    let fixture = Fixture::new();
    let (base, first, second) = two_revisions(&fixture).await;
    let (first_dir, first_git) = bare();
    let first_export = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        &first_git,
        &ExportOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(first_export.projected.len(), 2);
    assert_eq!(
        first_export.projected[0].application_id,
        first.application_id
    );
    assert_eq!(
        first_export.projected[1].application_id,
        second.application_id
    );
    assert_eq!(first_export.tip, Some(first_export.projected[1].commit_oid));
    let tip = first_git.find_commit(first_export.tip.unwrap()).unwrap();
    assert_eq!(tip.message_raw().unwrap(), b"second revision\n");
    assert_eq!(tip.parent_ids().count(), 1);
    assert_eq!(
        first_export.views[&first.resulting_view],
        first_export.revisions[&first.revision_id]
    );

    let (second_dir, second_git) = bare();
    let second_export = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        &second_git,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(first_export.tip, second_export.tip);
    assert_eq!(first_export.projected, second_export.projected);

    git(first_dir.path(), &["fsck", "--full", "--no-dangling"]);
    git(second_dir.path(), &["fsck", "--full", "--no-dangling"]);
}

#[tokio::test]
async fn empty_revision_metadata_uses_stable_caller_fallback() {
    let fixture = Fixture::new();
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let revision = writer
        .write(
            base,
            proposal(
                1,
                None,
                "file",
                None,
                Some(value(&fixture.content, b"content", FileKind::Regular)),
                RevisionMetadata::default(),
            ),
        )
        .await
        .unwrap();
    let (_git_dir, git_repo) = bare();
    let options = ExportOptions::default();
    let exported = export_revision_history(
        &fixture.objects,
        &fixture.content,
        revision.resulting_view,
        base,
        &git_repo,
        &options,
    )
    .await
    .unwrap();
    let commit = git_repo.find_commit(exported.tip.unwrap()).unwrap();
    let decoded = commit.decode().unwrap();
    assert_eq!(
        decoded.message.as_bytes(),
        options.fallback.message.as_bytes()
    );
    let author = commit.author().unwrap();
    assert_eq!(author.name, options.fallback.actor.name.as_bytes());
    assert_eq!(author.email, options.fallback.actor.email.as_bytes());
    assert_eq!(
        author.time().unwrap().seconds,
        options.fallback.timestamp.seconds
    );
}

#[tokio::test]
async fn reuses_imported_anchor_and_projects_only_its_suffix() {
    let fixture = Fixture::new();
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let (_git_dir, git_repo) = bare();
    let anchor_blob = git_repo
        .write_object(&gix::objs::Blob {
            data: b"imported\n".to_vec(),
        })
        .unwrap()
        .detach();
    let anchor_tree = git_repo
        .write_object(&gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: "file.txt".into(),
                oid: anchor_blob,
            }],
        })
        .unwrap()
        .detach();
    let signature = gix::actor::Signature {
        name: "Imported".into(),
        email: "imported@example.com".into(),
        time: gix::date::Time {
            seconds: 1_600_000_000,
            offset: 0,
        },
    };
    let anchor = git_repo
        .write_object(&gix::objs::Commit {
            tree: anchor_tree,
            parents: Default::default(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: "imported\n".into(),
            extra_headers: Vec::new(),
        })
        .unwrap()
        .detach();

    let imported_value = value(&fixture.content, b"imported\n", FileKind::Regular);
    let mut imported_metadata = metadata("imported\n", 1_600_000_000);
    imported_metadata.provenance = Some(SourceProvenance {
        system: "git-sha1".into(),
        object_id: anchor.as_bytes().to_vec(),
        parent_object_ids: Vec::new(),
    });
    let imported = writer
        .write(
            base,
            proposal(
                1,
                None,
                "file.txt",
                None,
                Some(imported_value),
                imported_metadata,
            ),
        )
        .await
        .unwrap();
    let native = writer
        .write(
            imported.resulting_view,
            proposal(
                2,
                Some(imported.change_id),
                "file.txt",
                Some(imported_value),
                Some(value(&fixture.content, b"native\n", FileKind::Regular)),
                metadata("native\n", 1_700_000_000),
            ),
        )
        .await
        .unwrap();

    let exported = export_revision_history(
        &fixture.objects,
        &fixture.content,
        native.resulting_view,
        base,
        &git_repo,
        &ExportOptions {
            anchor: Some(anchor),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(exported.projected.len(), 2);
    assert!(exported.projected[0].reused_anchor);
    assert_eq!(exported.projected[0].commit_oid, anchor);
    assert!(!exported.projected[1].reused_anchor);
    let native_commit = git_repo.find_commit(exported.tip.unwrap()).unwrap();
    assert_eq!(native_commit.parent_ids().next().unwrap().detach(), anchor);
}

#[tokio::test]
async fn rejects_a_projected_anchor_whose_tree_does_not_match_its_application() {
    let fixture = Fixture::new();
    let (base, first, second) = two_revisions(&fixture).await;
    let (_git_dir, git_repo) = bare();
    let initial = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        &git_repo,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    assert!(initial.applications.contains_key(&first.application_id));
    let wrong_tree = git_repo
        .write_object(&gix::objs::Tree {
            entries: Vec::new(),
        })
        .unwrap()
        .detach();
    let signature = gix::actor::Signature {
        name: "Forged".into(),
        email: "forged@example.invalid".into(),
        time: gix::date::Time {
            seconds: 1_700_000_000,
            offset: 0,
        },
    };
    let forged = git_repo
        .write_object(&gix::objs::Commit {
            tree: wrong_tree,
            parents: Default::default(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: "forged mapping\n".into(),
            extra_headers: Vec::new(),
        })
        .unwrap()
        .detach();

    let error = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        &git_repo,
        &ExportOptions {
            anchor: Some(forged),
            projected_anchors: BTreeMap::from([(first.application_id, forged)]),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ExportError::MismatchedProjectedAnchor {
            application,
            anchor,
            actual_tree,
            expected_tree,
        } if application == first.application_id
            && anchor == forged
            && actual_tree == wrong_tree
            && expected_tree != wrong_tree
    ));
}

#[tokio::test]
async fn emits_regular_executable_symlink_and_binary_blobs() {
    let fixture = Fixture::new();
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let regular = value(&fixture.content, b"regular\n", FileKind::Regular);
    let first = writer
        .write(
            base,
            proposal(
                1,
                None,
                "regular",
                None,
                Some(regular),
                metadata("regular", 1),
            ),
        )
        .await
        .unwrap();
    let executable = value(
        &fixture.content,
        b"#!/bin/sh\necho ok\n",
        FileKind::Executable,
    );
    let second = writer
        .write(
            first.resulting_view,
            proposal(
                2,
                Some(first.change_id),
                "script",
                None,
                Some(executable),
                metadata("script", 2),
            ),
        )
        .await
        .unwrap();
    let symlink = value(&fixture.content, b"regular", FileKind::Symlink);
    let third = writer
        .write(
            second.resulting_view,
            proposal(
                3,
                Some(second.change_id),
                "link",
                None,
                Some(symlink),
                metadata("link", 3),
            ),
        )
        .await
        .unwrap();
    let binary = value(&fixture.content, b"nul\0raw\r\n\xff", FileKind::Regular);
    let fourth = writer
        .write(
            third.resulting_view,
            proposal(
                4,
                Some(third.change_id),
                "binary",
                None,
                Some(binary),
                metadata("binary", 4),
            ),
        )
        .await
        .unwrap();
    let (git_dir, git_repo) = bare();
    let exported = export_revision_history(
        &fixture.objects,
        &fixture.content,
        fourth.resulting_view,
        base,
        &git_repo,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let tree = git(
        git_dir.path(),
        &["ls-tree", exported.tip.unwrap().to_string().as_str()],
    );
    assert!(tree.contains("100644 blob") && tree.contains("\tregular"));
    assert!(tree.contains("100755 blob") && tree.contains("\tscript"));
    assert!(tree.contains("120000 blob") && tree.contains("\tlink"));
    let binary_oid = tree
        .lines()
        .find(|line| line.ends_with("\tbinary"))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap();
    let bytes = Command::new("git")
        .arg("-C")
        .arg(git_dir.path())
        .args(["cat-file", "blob", binary_oid])
        .output()
        .unwrap();
    assert!(bytes.status.success());
    assert_eq!(bytes.stdout, b"nul\0raw\r\n\xff");
    git(git_dir.path(), &["fsck", "--full", "--no-dangling"]);
}

#[tokio::test]
async fn rejects_a_selected_view_with_unresolved_conflicts() {
    let fixture = Fixture::new();
    let (base, _first, second) = two_revisions(&fixture).await;
    let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
    let selected = reader.view(second.resulting_view).await.unwrap();
    let conflict = Conflict::new(second.resulting_view, BTreeMap::new());
    let conflict_id = conflict.id;
    fixture
        .objects
        .put_if_absent(conflict_id.into(), &conflict.canonical_bytes())
        .await
        .unwrap();
    let conflicts = ConflictSetStore::new(&fixture.objects)
        .insert(selected.unresolved_conflicts, conflict_id)
        .await
        .unwrap();
    let conflicted = ViewRecord {
        unresolved_conflicts: conflicts,
        ..selected
    };
    fixture
        .objects
        .put_if_absent(conflicted.id().into(), &conflicted.canonical_bytes())
        .await
        .unwrap();
    let (_git_dir, git_repo) = bare();
    assert!(matches!(
        export_revision_history(&fixture.objects, &fixture.content, conflicted.id(), base, &git_repo, &ExportOptions::default()).await,
        Err(ExportError::ConflictedView(id)) if id == conflicted.id()
    ));
}
