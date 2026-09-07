use std::{path::Path, process::Command};

use wheat_core::{EditOperation, FileKind};
use wheat_git::{
    ImportError, ImportLimit, ImportLimits, import_repository, import_repository_with_limits,
};
use wheat_store::ContentStore;

fn git(repo: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Alice Author")
        .env("GIT_AUTHOR_EMAIL", "alice@example.com")
        .env("GIT_COMMITTER_NAME", "Casey Committer")
        .env("GIT_COMMITTER_EMAIL", "casey@example.com")
        .env("GIT_AUTHOR_DATE", "2024-01-02T03:04:05+02:00")
        .env("GIT_COMMITTER_DATE", "2024-01-02T04:05:06-03:00")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture() -> (tempfile::TempDir, String, String, String) {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    std::fs::write(directory.path().join("base.txt"), "base").unwrap();
    git(directory.path(), &["add", "."]);
    git(
        directory.path(),
        &["commit", "-m", "Root title", "-m", "Root description"],
    );
    let root = git(directory.path(), &["rev-parse", "HEAD"]);

    git(directory.path(), &["checkout", "-b", "feature"]);
    std::fs::write(directory.path().join("feature.txt"), "feature").unwrap();
    git(directory.path(), &["add", "."]);
    git(directory.path(), &["commit", "-m", "Feature"]);
    let feature = git(directory.path(), &["rev-parse", "HEAD"]);

    git(directory.path(), &["checkout", "main"]);
    std::fs::write(directory.path().join("main.txt"), "main").unwrap();
    git(directory.path(), &["add", "."]);
    git(directory.path(), &["commit", "-m", "Main"]);
    git(
        directory.path(),
        &["merge", "--no-ff", "feature", "-m", "Merge feature"],
    );
    let merge = git(directory.path(), &["rev-parse", "HEAD"]);
    (directory, root, feature, merge)
}

#[test]
fn imports_root_linear_branch_and_merge_with_first_parent_semantics() {
    let (fixture, root_oid, feature_oid, merge_oid) = fixture();
    let content_dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(content_dir.path());
    let imported = import_repository(fixture.path(), &content).unwrap();

    assert_eq!(imported.commit_count, 4);
    assert_eq!(imported.merge_count, 1);
    assert_eq!(imported.branches.len(), 2);
    let merge = &imported.commits[&hex(&merge_oid)];
    let feature = &imported.commits[&hex(&feature_oid)];
    let root = &imported.commits[&hex(&root_oid)];
    let merge_revision = imported.repository.revision(merge.revision_id).unwrap();
    let parents = &merge_revision
        .metadata()
        .provenance
        .as_ref()
        .unwrap()
        .parent_object_ids;

    assert_eq!(merge_revision.dependencies().len(), 1);
    assert_eq!(parents.len(), 2);
    assert_eq!(parents[1], hex(&feature_oid));
    assert_ne!(merge.change_id, feature.change_id);
    assert!(root.dependencies(&imported.repository).is_empty());
    assert_eq!(
        imported
            .repository
            .view(merge.resulting_view)
            .unwrap()
            .tree
            .len(),
        3
    );
    let root_tree = &imported.repository.view(root.resulting_view).unwrap().tree;
    assert_eq!(root_tree.len(), 1);
    assert!(root_tree.contains_key("base.txt"));
    assert!(!root_tree.contains_key("feature.txt"));
    assert!(!root_tree.contains_key("main.txt"));

    let feature_branch = &imported.branches["feature"].tip;
    let feature_tree = &imported
        .repository
        .view(feature_branch.resulting_view)
        .unwrap()
        .tree;
    assert_eq!(feature_tree.len(), 2);
    assert!(feature_tree.contains_key("feature.txt"));
    assert!(!feature_tree.contains_key("main.txt"));
}

#[test]
fn preserves_change_and_revision_metadata_and_content() {
    let (fixture, root_oid, _, _) = fixture();
    let content_dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(content_dir.path());
    let imported = import_repository(fixture.path(), &content).unwrap();
    let root = &imported.commits[&hex(&root_oid)];
    let change = imported.repository.change(root.change_id).unwrap();
    let revision = imported.repository.revision(root.revision_id).unwrap();

    assert_eq!(change.metadata.title, "Root title");
    assert_eq!(change.metadata.description, "Root description\n");
    assert_eq!(change.metadata.creator.name, "Alice Author");
    assert_eq!(change.metadata.created_at.seconds, 1_704_157_445);
    assert_eq!(change.metadata.created_at.offset_minutes, 120);
    assert_eq!(revision.metadata().committer.name, "Casey Committer");
    assert_eq!(revision.metadata().committed_at.offset_minutes, -180);
    assert_eq!(
        revision.metadata().provenance.as_ref().unwrap().system,
        "git-sha1"
    );
    let tree = imported.repository.view(root.resulting_view).unwrap();
    let bytes = content
        .get(tree.tree.get("base.txt").unwrap().content)
        .unwrap();
    assert_eq!(bytes, b"base");
}

#[test]
fn imports_file_kinds_and_canonicalizes_only_text_blobs() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    std::fs::write(directory.path().join("text"), b"one\r\ntwo\rthree\n").unwrap();
    std::fs::write(directory.path().join("script"), b"#!/bin/sh\r\necho ok\r").unwrap();
    std::fs::write(directory.path().join("binary"), b"nul\0raw\r\n\xff").unwrap();
    git(directory.path(), &["add", "."]);
    git(directory.path(), &["update-index", "--chmod=+x", "script"]);
    let shared_oid = git(directory.path(), &["hash-object", "text"]);
    git(
        directory.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{shared_oid},link"),
        ],
    );
    git(directory.path(), &["commit", "-m", "mixed files"]);
    let first = git(directory.path(), &["rev-parse", "HEAD"]);
    git(directory.path(), &["update-index", "--chmod=+x", "text"]);
    git(directory.path(), &["commit", "-m", "chmod only"]);
    let second = git(directory.path(), &["rev-parse", "HEAD"]);

    let content_dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(content_dir.path());
    let imported = import_repository(directory.path(), &content).unwrap();
    let first_commit = &imported.commits[&hex(&first)];
    let second_commit = &imported.commits[&hex(&second)];
    let first_tree = &imported
        .repository
        .view(first_commit.resulting_view)
        .unwrap()
        .tree;
    let second_tree = &imported
        .repository
        .view(second_commit.resulting_view)
        .unwrap()
        .tree;

    assert_eq!(first_tree["text"].kind, FileKind::Regular);
    assert_eq!(first_tree["script"].kind, FileKind::Executable);
    assert_eq!(first_tree["link"].kind, FileKind::Symlink);
    assert_eq!(second_tree["text"].kind, FileKind::Executable);
    assert_eq!(first_tree["text"].content, second_tree["text"].content);
    assert_eq!(
        content.get(first_tree["text"].content).unwrap(),
        b"one\ntwo\nthree\n"
    );
    assert_eq!(
        content.get(first_tree["script"].content).unwrap(),
        b"#!/bin/sh\necho ok\n"
    );
    assert_eq!(
        content.get(first_tree["link"].content).unwrap(),
        b"one\r\ntwo\rthree\n"
    );
    assert_eq!(
        content.get(first_tree["binary"].content).unwrap(),
        b"nul\0raw\r\n\xff"
    );
    assert_ne!(first_tree["text"].content, first_tree["link"].content);

    let revision = imported
        .repository
        .revision(second_commit.revision_id)
        .unwrap();
    assert!(!revision.edits().is_empty());
    assert!(revision.edits().values().all(|edit| edit.path == "text"));
    assert!(revision.edits().values().any(|edit| matches!(
        edit.operation,
        EditOperation::ExactFile {
            expected: Some(_),
            replacement: Some(_)
        }
    )));
    assert_eq!(
        revision.edit_transition().added,
        revision.edits().keys().copied().collect()
    );
}

#[test]
fn direct_import_rejects_commit_limit_before_graph_accumulates() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    for number in 0..3 {
        std::fs::write(directory.path().join("file"), number.to_string()).unwrap();
        git(directory.path(), &["add", "file"]);
        git(directory.path(), &["commit", "-m", "next"]);
    }
    let content_dir = tempfile::tempdir().unwrap();
    let limits = ImportLimits {
        max_reachable_commits: 2,
        ..ImportLimits::default()
    };
    assert!(matches!(
        import_repository_with_limits(
            directory.path(),
            &ContentStore::new(content_dir.path()),
            limits
        ),
        Err(ImportError::LimitExceeded {
            limit: ImportLimit::ReachableCommits,
            ..
        })
    ));
}

#[test]
fn direct_import_rejects_flat_tree_budget_before_materializing_more_entries() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    std::fs::write(directory.path().join("one"), b"one").unwrap();
    std::fs::write(directory.path().join("two"), b"two").unwrap();
    git(directory.path(), &["add", "."]);
    git(directory.path(), &["commit", "-m", "tree"]);
    let content_dir = tempfile::tempdir().unwrap();
    let limits = ImportLimits {
        max_flat_tree_entries: 1,
        ..ImportLimits::default()
    };
    assert!(matches!(
        import_repository_with_limits(
            directory.path(),
            &ContentStore::new(content_dir.path()),
            limits
        ),
        Err(ImportError::LimitExceeded {
            limit: ImportLimit::FlatTreeEntries,
            ..
        })
    ));
}

#[test]
fn direct_import_rejects_pending_content_budget_before_copying_more_content() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    std::fs::write(directory.path().join("file"), b"too much content").unwrap();
    git(directory.path(), &["add", "file"]);
    git(directory.path(), &["commit", "-m", "content"]);
    let content_dir = tempfile::tempdir().unwrap();
    let limits = ImportLimits {
        max_pending_content_bytes: 4,
        ..ImportLimits::default()
    };
    assert!(matches!(
        import_repository_with_limits(
            directory.path(),
            &ContentStore::new(content_dir.path()),
            limits
        ),
        Err(ImportError::LimitExceeded {
            limit: ImportLimit::PendingContentBytes,
            ..
        })
    ));
}

#[test]
fn repeat_import_is_deterministic() {
    let (fixture, _, _, _) = fixture();
    let first_content = tempfile::tempdir().unwrap();
    let second_content = tempfile::tempdir().unwrap();
    let first =
        import_repository(fixture.path(), &ContentStore::new(first_content.path())).unwrap();
    let second =
        import_repository(fixture.path(), &ContentStore::new(second_content.path())).unwrap();

    assert_eq!(first.commits, second.commits);
    assert_eq!(
        first.repository.canonical_closure(),
        second.repository.canonical_closure()
    );
}

fn hex(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).unwrap())
        .collect()
}

trait ImportedCommitExt {
    fn dependencies<'a>(
        &self,
        repository: &'a wheat_core::Repository,
    ) -> &'a std::collections::BTreeSet<wheat_core::ChangeId>;
}

impl ImportedCommitExt for wheat_git::ImportedCommit {
    fn dependencies<'a>(
        &self,
        repository: &'a wheat_core::Repository,
    ) -> &'a std::collections::BTreeSet<wheat_core::ChangeId> {
        repository
            .revision(self.revision_id)
            .unwrap()
            .dependencies()
    }
}
