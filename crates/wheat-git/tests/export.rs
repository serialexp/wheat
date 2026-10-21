use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

use gix::bstr::ByteSlice;
use wheat_core::{
    Actor, Change, ChangeId, ChangeMapId, ChangeMetadata, ChangeObjectId, ChangeProposal, Conflict,
    EditId, EditOperation, EditRecord, EditTransition, FileKind, FileValue, LabelAnnotation,
    ProjectedCommit, Repository, RevisionEdits, RevisionMetadata, RevisionOutcome,
    SourceProvenance, Timestamp, Tree, ViewId, ViewRecord,
};
use wheat_git::export::{
    ExportAnchor, ExportError, ExportLimit, ExportOptions, ExportWork, export_revision_history,
    write_tag,
};
use wheat_store::{
    ChangeMapStore, ConflictSetStore, ContentStore, ImmutableObjectStore, LocalPublicationStore,
    LooseObjectStore, PersistedCommit, PersistedCommitWriter, PersistedRepositoryMutator,
    ProjectedCommitSetStore, RepositoryPublisher, RepositoryReader,
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

/// The change map for changes written by [`proposal`], whose change metadata is empty.
async fn default_changes(fixture: &Fixture, numbers: &[u8]) -> ChangeMapId {
    let store = ChangeMapStore::new(&fixture.objects);
    let mut root = store.empty_root().await.unwrap();
    for number in numbers {
        let change = Change {
            id: ChangeId::from_bytes([*number; 16]),
            metadata: Default::default(),
        };
        root = store
            .insert(
                root,
                change.id,
                ChangeObjectId::of_canonical_bytes(&change.canonical_bytes()),
            )
            .await
            .unwrap();
    }
    root
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
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (first_dir, first_git) = bare();
    let first_export = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
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
        changes,
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
        default_changes(&fixture, &[1]).await,
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
        default_changes(&fixture, &[1, 2]).await,
        &git_repo,
        &ExportOptions {
            anchor: ExportAnchor::Commit(anchor),
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

    // The fixed tag rule finds the same anchor from the view alone, so it writes the
    // same commits a branch push anchored on the remote tip wrote.
    let changes = default_changes(&fixture, &[1, 2]).await;
    let newest_imported = ExportOptions {
        anchor: ExportAnchor::NewestImported,
        ..Default::default()
    };
    let fixed = export_revision_history(
        &fixture.objects,
        &fixture.content,
        native.resulting_view,
        base,
        changes,
        &git_repo,
        &newest_imported,
    )
    .await
    .unwrap();
    assert_eq!(fixed.projected, exported.projected);

    // Without the imported commit present, the rule fails instead of silently
    // writing a different history.
    let (_empty_dir, empty) = bare();
    assert!(matches!(
        export_revision_history(
            &fixture.objects,
            &fixture.content,
            native.resulting_view,
            base,
            changes,
            &empty,
            &newest_imported,
        )
        .await,
        Err(ExportError::MissingAnchor(missing)) if missing == anchor
    ));
}

#[tokio::test]
async fn newest_imported_is_a_fresh_export_for_native_history() {
    let fixture = Fixture::new();
    let (base, _first, second) = two_revisions(&fixture).await;
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (_plain_dir, plain_git) = bare();
    let plain = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &plain_git,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let (_fixed_dir, fixed_git) = bare();
    let fixed = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &fixed_git,
        &ExportOptions {
            anchor: ExportAnchor::NewestImported,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(fixed.projected, plain.projected);
    assert!(fixed.projected.iter().all(|entry| !entry.reused_anchor));
}

/// A view's commit must be the same whether it is exported alone or as the parent of
/// a later view, or a tag on it would not name the commit already on the branch.
#[tokio::test]
async fn an_earlier_view_exports_to_the_parent_commit_of_a_later_view() {
    let fixture = Fixture::new();
    let (base, first, second) = two_revisions(&fixture).await;
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (_later_dir, later_git) = bare();
    let later = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &later_git,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let (_earlier_dir, earlier_git) = bare();
    let earlier = export_revision_history(
        &fixture.objects,
        &fixture.content,
        first.resulting_view,
        base,
        changes,
        &earlier_git,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let parent = later_git
        .find_commit(later.tip.unwrap())
        .unwrap()
        .parent_ids()
        .next()
        .unwrap()
        .detach();
    assert_eq!(earlier.tip, Some(parent));
    assert_eq!(earlier.projected[..], later.projected[..1]);
}

fn annotation(message: &str) -> LabelAnnotation {
    LabelAnnotation {
        tagger: Actor {
            name: "Tia Tagger".into(),
            email: "tia@example.com".into(),
        },
        tagged_at: Timestamp {
            seconds: 1_790_000_000,
            offset_minutes: -330,
        },
        message: message.into(),
    }
}

#[tokio::test]
async fn a_tag_object_depends_only_on_commit_name_and_annotation() {
    let fixture = Fixture::new();
    let (base, _first, second) = two_revisions(&fixture).await;
    let changes = default_changes(&fixture, &[1, 2]).await;
    let mut tags = Vec::new();
    let mut directories = Vec::new();
    for _ in 0..2 {
        let (directory, git_repo) = bare();
        let export = export_revision_history(
            &fixture.objects,
            &fixture.content,
            second.resulting_view,
            base,
            changes,
            &git_repo,
            &ExportOptions::default(),
        )
        .await
        .unwrap();
        let tag = write_tag(
            &git_repo,
            export.tip.unwrap(),
            "v1.0",
            &annotation("Release 1.0\n\nNotes."),
        )
        .unwrap();
        git_repo
            .reference(
                "refs/tags/v1.0",
                tag,
                gix::refs::transaction::PreviousValue::MustNotExist,
                "test",
            )
            .unwrap();
        tags.push((tag, export.tip.unwrap()));
        directories.push(directory);
    }
    assert_eq!(tags[0], tags[1]);

    let path = directories[0].path();
    git(path, &["fsck", "--full", "--strict", "--no-dangling"]);
    assert_eq!(
        git(path, &["rev-parse", "v1.0^{commit}"]),
        tags[0].1.to_string()
    );
    assert_eq!(
        git(path, &["cat-file", "tag", "v1.0"]),
        format!(
            "object {}\ntype commit\ntag v1.0\ntagger Tia Tagger <tia@example.com> 1790000000 -0530\n\nRelease 1.0\n\nNotes.",
            tags[0].1
        )
    );

    // Each stored field is part of the ID; a message without a final newline gets one.
    let (_other_dir, other) = bare();
    let commit = tags[0].1;
    let base_tag = write_tag(&other, commit, "v1.0", &annotation("Release 1.0\n\nNotes.")).unwrap();
    assert_eq!(base_tag, tags[0].0);
    assert_eq!(
        write_tag(
            &other,
            commit,
            "v1.0",
            &annotation("Release 1.0\n\nNotes.\n")
        )
        .unwrap(),
        base_tag
    );
    let mut later = annotation("Release 1.0\n\nNotes.");
    later.tagged_at.seconds += 1;
    for different in [
        write_tag(&other, commit, "v1.1", &annotation("Release 1.0\n\nNotes.")).unwrap(),
        write_tag(&other, commit, "v1.0", &annotation("Release 1.0")).unwrap(),
        write_tag(&other, commit, "v1.0", &later).unwrap(),
    ] {
        assert_ne!(different, base_tag);
    }
}

#[tokio::test]
async fn reuses_a_native_anchor_only_when_the_projected_commits_name_it() {
    let fixture = Fixture::new();
    let (base, first, second) = two_revisions(&fixture).await;
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (_git_dir, git_repo) = bare();
    let initial = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &git_repo,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let pushed = initial.applications[&first.application_id];
    let sets = ProjectedCommitSetStore::new(&fixture.objects);
    let empty = sets.empty_root().await.unwrap();
    let recorded = sets
        .insert(
            empty,
            ProjectedCommit::new(pushed.as_bytes().try_into().unwrap(), first.application_id),
        )
        .await
        .unwrap();
    let export = |projected_commits| {
        let git_repo = &git_repo;
        let fixture = &fixture;
        async move {
            export_revision_history(
                &fixture.objects,
                &fixture.content,
                second.resulting_view,
                base,
                changes,
                git_repo,
                &ExportOptions {
                    anchor: ExportAnchor::Commit(pushed),
                    projected_commits: Some(projected_commits),
                    ..Default::default()
                },
            )
            .await
        }
    };

    let reused = export(recorded).await.unwrap();
    assert!(reused.projected[0].reused_anchor);
    assert_eq!(reused.projected[0].application_id, first.application_id);
    assert_eq!(reused.projected.len(), 2);
    // Exports are deterministic, so the commit above the anchor is the one written
    // before.
    assert_eq!(reused.tip, initial.tip);

    // A native commit no record names is not this history's.
    assert!(matches!(
        export(empty).await.unwrap_err(),
        ExportError::UnmatchedAnchor(anchor) if anchor == pushed
    ));
}

#[tokio::test]
async fn a_projected_anchor_seeds_the_next_tree_so_only_changed_paths_are_read() {
    const DIRECTORIES: usize = 16;
    const FILES: usize = 8;
    let fixture = Fixture::new();
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let mut edits = BTreeMap::new();
    for directory in 0..DIRECTORIES {
        for file in 0..FILES {
            let path = format!("d{directory}/f{file}.txt");
            let replacement = value(
                &fixture.content,
                format!("{path}\n").as_bytes(),
                FileKind::Regular,
            );
            let edit = EditRecord::new(
                &path,
                EditOperation::ExactFile {
                    expected: None,
                    replacement: Some(replacement),
                },
            )
            .unwrap();
            edits.insert(edit.id(), edit);
        }
    }
    let added = edits.keys().copied().collect();
    let wide = ChangeProposal {
        id: ChangeId::from_bytes([1; 16]),
        metadata: Default::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: metadata("many files\n", 1_700_000_000),
        edits: RevisionEdits {
            edits,
            transition: EditTransition {
                added,
                ..Default::default()
            },
        },
    };
    let first = writer.write(base, wide).await.unwrap();
    let changed = "d3/f5.txt";
    let second = writer
        .write(
            first.resulting_view,
            proposal(
                2,
                Some(first.change_id),
                changed,
                Some(value(
                    &fixture.content,
                    format!("{changed}\n").as_bytes(),
                    FileKind::Regular,
                )),
                Some(value(&fixture.content, b"changed\n", FileKind::Regular)),
                metadata("one file\n", 1_700_000_100),
            ),
        )
        .await
        .unwrap();
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (_complete_dir, complete_repo) = bare();
    let complete = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &complete_repo,
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    let pushed = complete.applications[&first.application_id];
    let sets = ProjectedCommitSetStore::new(&fixture.objects);
    let recorded = sets
        .insert(
            sets.empty_root().await.unwrap(),
            ProjectedCommit::new(pushed.as_bytes().try_into().unwrap(), first.application_id),
        )
        .await
        .unwrap();
    // A projection holding only the pushed commit, as one fetched from the remote.
    let (_seeded_dir, seeded_repo) = bare();
    git(
        seeded_repo.path(),
        &[
            "fetch",
            complete_repo.path().to_str().unwrap(),
            &format!("{pushed}:refs/heads/pushed"),
        ],
    );

    let seeded = export_revision_history(
        &fixture.objects,
        &fixture.content,
        second.resulting_view,
        base,
        changes,
        &seeded_repo,
        &ExportOptions {
            anchor: ExportAnchor::Commit(pushed),
            projected_commits: Some(recorded),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The commit is the one a complete export writes, read from the changed directory
    // and the root, each with the seed's, and the changed file: not from every
    // directory and file.
    assert_eq!(seeded.tip, complete.tip);
    assert_eq!(
        seeded.work,
        ExportWork {
            trees_read: 4,
            contents_read: 1,
        }
    );
    // The first commit of a complete export has no seed: every file is read.
    assert_eq!(complete.work.contents_read, DIRECTORIES * FILES + 1);
    assert_eq!(
        git(seeded_repo.path(), &["fsck", "--strict", "--no-dangling"]),
        ""
    );
}

#[tokio::test]
async fn a_base_the_selected_view_does_not_descend_from_is_disconnected() {
    let fixture = Fixture::new();
    let (_, first, second) = two_revisions(&fixture).await;
    let changes = default_changes(&fixture, &[1, 2]).await;
    let (_git_dir, git_repo) = bare();

    let error = export_revision_history(
        &fixture.objects,
        &fixture.content,
        first.resulting_view,
        second.resulting_view,
        changes,
        &git_repo,
        &ExportOptions::default(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            error,
            ExportError::Disconnected { selected, base }
                if selected == first.resulting_view && base == second.resulting_view
        ),
        "{error}"
    );
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
        default_changes(&fixture, &[1, 2, 3, 4]).await,
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
    let changes = default_changes(&fixture, &[1, 2]).await;
    assert!(matches!(
        export_revision_history(&fixture.objects, &fixture.content, conflicted.id(), base, changes, &git_repo, &ExportOptions::default()).await,
        Err(ExportError::ConflictedView(id)) if id == conflicted.id()
    ));
}

/// One native change with one checkpoint per note, each rewriting `file`, authored the way
/// `wheat change new -m` and `wheat add` do. Returns the base, the latest view, and the
/// repository state's change map.
async fn native_history(
    fixture: &Fixture,
    title: &str,
    description: &str,
    notes: &[&str],
) -> (ViewId, ViewId, ChangeMapId) {
    let heads = LocalPublicationStore::new(fixture._directory.path().join("head"));
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut head = RepositoryPublisher::new(&fixture.objects, &fixture.content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    let mutator = PersistedRepositoryMutator::new(&fixture.objects, &fixture.content, &heads);
    let change = ChangeId::new();
    let mut previous: Option<(RevisionOutcome, EditId)> = None;
    for (index, note) in notes.iter().enumerate() {
        let bytes = format!("version {index}\n");
        let edit = EditRecord::new(
            "file",
            EditOperation::ExactFile {
                expected: None,
                replacement: Some(value(&fixture.content, bytes.as_bytes(), FileKind::Regular)),
            },
        )
        .unwrap();
        let id = edit.id();
        let mut edits = RevisionEdits {
            edits: BTreeMap::from([(id, edit)]),
            transition: EditTransition {
                added: [id].into(),
                ..Default::default()
            },
        };
        let revision_metadata = metadata(note, 1_700_000_000 + index as i64);
        let (prepared, outcome) = match &previous {
            None => mutator
                .create(
                    head,
                    base,
                    base,
                    ChangeProposal {
                        id: change,
                        metadata: ChangeMetadata {
                            title: title.into(),
                            description: description.into(),
                            ..Default::default()
                        },
                        dependencies: BTreeSet::new(),
                        revision_metadata,
                        edits,
                    },
                )
                .await
                .unwrap(),
            Some((outcome, previous_edit)) => {
                edits.transition.removed.insert(*previous_edit);
                mutator
                    .revise(
                        head,
                        base,
                        outcome.resulting_view,
                        change,
                        outcome.revision_id,
                        revision_metadata,
                        edits,
                    )
                    .await
                    .unwrap()
            }
        };
        head = mutator.publish(prepared).await.unwrap();
        previous = Some((outcome, id));
    }
    let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let state = reader.state(envelope.state).await.unwrap();
    (base, previous.unwrap().0.resulting_view, state.changes)
}

async fn exported_message(
    fixture: &Fixture,
    history: (ViewId, ViewId, ChangeMapId),
    options: &ExportOptions,
) -> Result<(String, gix::actor::Signature), ExportError> {
    let (base, latest, changes) = history;
    let (_git_dir, git_repo) = bare();
    let exported = export_revision_history(
        &fixture.objects,
        &fixture.content,
        latest,
        base,
        changes,
        &git_repo,
        options,
    )
    .await?;
    let commit = git_repo.find_commit(exported.tip.unwrap()).unwrap();
    let message = commit.message_raw().unwrap().to_str().unwrap().to_owned();
    let author = commit.author().unwrap().to_owned().unwrap();
    Ok((message, author))
}

#[tokio::test]
async fn a_native_commit_describes_the_change_rather_than_its_newest_checkpoint() {
    let fixture = Fixture::new();
    let history = native_history(
        &fixture,
        "Survive agent restarts",
        "",
        &[
            "Survive agent restarts\n\nContainers must outlive the agent.",
            "",
            "Fix the digest mode\n\nCo-authored-by: Alice <alice@example.invalid>",
            "",
        ],
    )
    .await;
    let (message, author) = exported_message(&fixture, history, &ExportOptions::default())
        .await
        .unwrap();
    assert_eq!(
        message,
        "Survive agent restarts\n\nContainers must outlive the agent.\n\nFix the digest mode\n\nCo-authored-by: Alice <alice@example.invalid>\n"
    );
    assert_eq!(author.email, "alice@example.com");
    assert_eq!(author.time.seconds, 1_700_000_003);
}

#[tokio::test]
async fn a_native_commit_includes_the_change_description() {
    let fixture = Fixture::new();
    let history = native_history(
        &fixture,
        "Initial import",
        "Everything so far.",
        &["Initial import", ""],
    )
    .await;
    let (message, _) = exported_message(&fixture, history, &ExportOptions::default())
        .await
        .unwrap();
    assert_eq!(message, "Initial import\n\nEverything so far.\n");
}

#[tokio::test]
async fn composing_a_message_is_bounded_by_checkpoint_reads_and_message_size() {
    let fixture = Fixture::new();
    let history = native_history(&fixture, "Title", "", &["Title", "", "", "Last note"]).await;
    let mut options = ExportOptions::default();
    options.limits.max_note_revisions = 2;
    assert!(matches!(
        exported_message(&fixture, history, &options).await,
        Err(ExportError::LimitExceeded {
            limit: ExportLimit::NoteRevisions,
            actual: 3,
            maximum: 2,
        })
    ));
    options.limits.max_note_revisions = 3;
    assert!(exported_message(&fixture, history, &options).await.is_ok());

    let mut options = ExportOptions::default();
    options.limits.max_message_bytes = "Title\n\nLast note".len();
    assert!(matches!(
        exported_message(&fixture, history, &options).await,
        Err(ExportError::LimitExceeded {
            limit: ExportLimit::MessageBytes,
            ..
        })
    ));
    options.limits.max_message_bytes += 1;
    assert_eq!(
        exported_message(&fixture, history, &options)
            .await
            .unwrap()
            .0,
        "Title\n\nLast note\n"
    );
}

#[tokio::test]
async fn an_imported_revision_keeps_its_message_byte_for_byte() {
    let fixture = Fixture::new();
    let writer = PersistedCommitWriter::new(&fixture.objects, &fixture.content);
    let base = writer.empty_view().await.unwrap();
    let raw = "imported title  \n\n\nbody with Co-authored-by: nobody\n\n";
    let mut imported_metadata = metadata(raw, 1_600_000_000);
    imported_metadata.provenance = Some(SourceProvenance {
        system: "git-sha1".into(),
        object_id: vec![7; 20],
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
                Some(value(&fixture.content, b"imported\n", FileKind::Regular)),
                imported_metadata,
            ),
        )
        .await
        .unwrap();
    let changes = default_changes(&fixture, &[1]).await;
    let (message, _) = exported_message(
        &fixture,
        (base, imported.resulting_view, changes),
        &ExportOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(message, raw);
}

#[tokio::test]
async fn a_change_missing_from_the_change_map_is_refused() {
    let fixture = Fixture::new();
    let (base, _first, second) = two_revisions(&fixture).await;
    let only_first = default_changes(&fixture, &[1]).await;
    assert!(matches!(
        exported_message(&fixture, (base, second.resulting_view, only_first), &ExportOptions::default()).await,
        Err(ExportError::UnknownChange(change)) if change == ChangeId::from_bytes([2; 16])
    ));
}
