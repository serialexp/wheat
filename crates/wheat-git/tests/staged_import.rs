use std::{path::Path, process::Command};
use wheat_git::{import_repository, stage_import};
use wheat_store::{ContentStore, LooseObjectStore, PackedObjectStore, RepositoryReader};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("file"), b"same").unwrap();
    std::fs::write(dir.path().join("duplicate"), b"same").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "root"]);
    git(dir.path(), &["checkout", "-b", "feature"]);
    git(dir.path(), &["rm", "duplicate"]);
    git(dir.path(), &["commit", "-m", "delete duplicate"]);
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("other"), b"other").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "main"]);
    git(dir.path(), &["merge", "--no-ff", "feature", "-m", "merge"]);
    dir
}

#[tokio::test]
async fn interrupted_staging_matches_reference_commit_identities() {
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");
    let reference = import_repository(git.path(), &content).unwrap();
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    // Probe every canonical view, not just the chosen branch tip. Matching IDs
    // imply equal canonical records and all historical selections/content roots.
    let reader = RepositoryReader::new(&objects, &content);
    for expected in reference.commits.values() {
        let revision = reader.revision(expected.revision_id).await.unwrap();
        assert_eq!(revision.change_id, expected.change_id);
        let view = reader.view(expected.resulting_view).await.unwrap();
        assert_eq!(view.id(), expected.resulting_view);
    }
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    assert!(
        !dir.path().join("repository.head").exists(),
        "staging must not publish"
    );
}

#[tokio::test]
async fn packed_restart_resume_matches_loose_reference_and_never_checkpoints_missing_objects() {
    let git = fixture();
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("import.sqlite");
    let reference_content = ContentStore::new(directory.path().join("reference-content"));
    let reference = import_repository(git.path(), &reference_content).unwrap();

    {
        let objects = PackedObjectStore::open(directory.path().join("packed")).unwrap();
        let content = objects.content_store();
        stage_import(git.path(), &journal, &objects, &content, Some(1))
            .await
            .unwrap();
    }
    let objects = PackedObjectStore::open(directory.path().join("packed")).unwrap();
    let content = objects.content_store();
    let resumed = stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    assert!(resumed.complete);

    // The journal cannot be authoritative: every completed mapping must reopen
    // solely from its durable segments and pass typed reads.
    let reader = RepositoryReader::new(&objects, &content);
    for expected in reference.commits.values() {
        assert_eq!(
            reader
                .revision(expected.revision_id)
                .await
                .unwrap()
                .change_id,
            expected.change_id
        );
        assert_eq!(
            reader.view(expected.resulting_view).await.unwrap().id(),
            expected.resulting_view
        );
    }
}

#[tokio::test]
async fn resume_rejects_changed_source_refs() {
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    self::git(git.path(), &["commit", "--allow-empty", "-m", "moved"]);
    assert!(
        stage_import(git.path(), &journal, &objects, &content, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn finalization_retains_branches_without_journal_and_refuses_partial_import() {
    use wheat_git::finalize_import;
    use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore};
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let journal = dir.path().join("import.sqlite");
    let reference = import_repository(git.path(), &content).unwrap();
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    assert!(
        finalize_import(
            git.path(),
            &journal,
            &objects,
            &content,
            &heads,
            "refs/heads/main"
        )
        .await
        .is_err()
    );
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    let head = finalize_import(
        git.path(),
        &journal,
        &objects,
        &content,
        &heads,
        "refs/heads/main",
    )
    .await
    .unwrap();
    assert_eq!(
        finalize_import(
            git.path(),
            &journal,
            &objects,
            &content,
            &heads,
            "refs/heads/main"
        )
        .await
        .unwrap(),
        head
    );
    std::fs::rename(&journal, dir.path().join("inspection.sqlite")).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let branches = reader.branch_manifest(envelope.branches).await.unwrap();
    assert_eq!(branches.branches.len(), 2);
    for (name, branch) in reference.branches {
        assert_eq!(
            branches.branches[&format!("refs/heads/{name}")],
            branch.tip.resulting_view
        );
    }
    let state = reader.state(envelope.state).await.unwrap();
    assert_eq!(state.working_view, branches.branches["refs/heads/main"]);
    reader.validate_complete(envelope.state).await.unwrap();
}
