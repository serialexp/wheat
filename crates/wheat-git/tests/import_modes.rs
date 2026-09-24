use std::{
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use wheat_git::{AtomicImportError, atomic_import, finalize_import, stage_import};
use wheat_store::{LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryReader};

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
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
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    std::fs::write(root.path().join("file"), b"one\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-m", "root"]);
    git(root.path(), &["checkout", "-b", "feature"]);
    std::fs::write(root.path().join("feature"), b"feature\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-m", "feature"]);
    git(root.path(), &["checkout", "main"]);
    std::fs::write(root.path().join("file"), b"two\n").unwrap();
    git(root.path(), &["commit", "-am", "main"]);
    root
}

fn repeated_edit_fixture(commits: usize) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    let mut content = (0..256)
        .map(|line| format!("line {line:04}: unchanged fixture content\n"))
        .collect::<String>();
    content.replace_range(0..4, "0000");
    std::fs::write(root.path().join("file"), &content).unwrap();
    git(root.path(), &["add", "file"]);
    git(root.path(), &["commit", "-m", "root"]);
    for revision in 1..=commits {
        content.replace_range(0..4, &format!("{revision:04}"));
        std::fs::write(root.path().join("file"), &content).unwrap();
        git(root.path(), &["commit", "-am", "repeated edit"]);
    }
    root
}

fn large_binary_fixture() -> (tempfile::TempDir, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    let mut content = vec![0_u8; 17 * 1024 * 1024];
    for (index, byte) in content.iter_mut().enumerate() {
        *byte = (index.wrapping_mul(31) >> 9) as u8;
    }
    std::fs::write(root.path().join("model.bin"), &content).unwrap();
    git(root.path(), &["add", "model.bin"]);
    git(root.path(), &["commit", "-m", "large binary"]);
    (root, content)
}

fn oversized_binary_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    let file = std::fs::File::create(root.path().join("too-large.bin")).unwrap();
    file.set_len(64 * 1024 * 1024 + 1).unwrap();
    git(root.path(), &["add", "too-large.bin"]);
    git(root.path(), &["commit", "-m", "oversized binary"]);
    root
}

fn private_stage(root: &Path, destination: &str, id: &str) -> PathBuf {
    root.join(format!(".{destination}.wheat-import-{id}.tmp"))
}

fn stage_names(root: &Path, destination: &str) -> Vec<String> {
    let prefix = format!(".{destination}.wheat-import-");
    let mut names = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&prefix) && name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn atomic_and_resumable_imports_publish_the_same_envelope() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let atomic = root.path().join("atomic");
    let resumable = root.path().join("resumable");

    let atomic_head = atomic_import(git.path(), &atomic, "refs/heads/main")
        .await
        .unwrap();

    std::fs::create_dir(&resumable).unwrap();
    let objects = PackedObjectStore::open(resumable.join("storage")).unwrap();
    let content = objects.content_store();
    let journal = resumable.join("import.sqlite");
    let publications = LocalPublicationStore::new(resumable.join("repository.head"));
    loop {
        let progress = stage_import(git.path(), &journal, &objects, &content, None)
            .await
            .unwrap();
        if progress.complete {
            break;
        }
    }
    let resumable_head = finalize_import(
        git.path(),
        &journal,
        &objects,
        &content,
        &publications,
        "refs/heads/main",
    )
    .await
    .unwrap();

    assert_eq!(atomic_head, resumable_head);
    let atomic_objects = PackedObjectStore::open(atomic.join("storage")).unwrap();
    let atomic_content = atomic_objects.content_store();
    let atomic_reader = RepositoryReader::new(&atomic_objects, &atomic_content);
    let atomic_envelope = atomic_reader.envelope(atomic_head.envelope).await.unwrap();
    atomic_reader
        .validate_complete(atomic_envelope.state)
        .await
        .unwrap();
    let labels = atomic_reader
        .view_label_manifest(atomic_envelope.labels)
        .await
        .unwrap();
    assert_eq!(labels.labels.len(), 2);
    assert_eq!(
        labels.labels["main"].original_git_ref.as_deref(),
        Some("refs/heads/main")
    );
    assert_eq!(
        LocalPublicationStore::new(atomic.join("repository.head"))
            .read()
            .unwrap(),
        atomic_head
    );
}

#[tokio::test]
async fn resumable_packed_import_reopens_before_authenticated_publication() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("resumable");
    std::fs::create_dir(&repository).unwrap();
    let storage = repository.join("storage");
    let journal = repository.join("import.sqlite");
    let publication_path = repository.join("repository.head");

    {
        let objects = PackedObjectStore::open(&storage).unwrap();
        let content = objects.content_store();
        let progress = stage_import(git.path(), &journal, &objects, &content, Some(1))
            .await
            .unwrap();
        assert!(!progress.complete);
        drop(content);
        objects.close().unwrap();
    }

    {
        let objects = PackedObjectStore::open(&storage).unwrap();
        let content = objects.content_store();
        loop {
            let progress = stage_import(git.path(), &journal, &objects, &content, None)
                .await
                .unwrap();
            if progress.complete {
                break;
            }
        }
        drop(content);
        objects.close().unwrap();
    }

    let head = {
        let objects = PackedObjectStore::open(&storage).unwrap();
        let content = objects.content_store();
        let publications = LocalPublicationStore::new(&publication_path);
        let head = finalize_import(
            git.path(),
            &journal,
            &objects,
            &content,
            &publications,
            "refs/heads/main",
        )
        .await
        .unwrap();
        assert_eq!(publications.read().unwrap(), head);
        drop(content);
        objects.close().unwrap();
        head
    };

    // Publication is trusted only after a fresh packed-store open authenticates
    // the active segment and every object reachable from the published envelope.
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
    assert_eq!(
        LocalPublicationStore::new(publication_path).read().unwrap(),
        head
    );
}

#[tokio::test]
async fn traced_atomic_import_reports_bounded_progress_and_ordered_phases() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("atomic");
    let mut events = Vec::new();

    wheat_git::atomic_import_with_observer(git.path(), &destination, "refs/heads/main", |event| {
        events.push(event)
    })
    .await
    .unwrap();

    let progress = events
        .iter()
        .filter_map(|event| match event {
            wheat_git::AtomicImportEvent::StageProgress { complete, .. } => Some(*complete),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(progress.last(), Some(&true));
    assert!(progress.len() <= 2);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                wheat_git::AtomicImportEvent::FinalizePhaseStarted { phase } => Some(*phase),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [
            wheat_git::FinalizePhase::Preflight,
            wheat_git::FinalizePhase::JournalAuthenticationAccumulation,
            wheat_git::FinalizePhase::AggregateSetConstruction,
            wheat_git::FinalizePhase::BatchFlush,
            wheat_git::FinalizePhase::PrepareEnvelopeValidation,
            wheat_git::FinalizePhase::HeadInitialize,
        ]
    );
    assert!(matches!(
        events.last(),
        Some(wheat_git::AtomicImportEvent::PhaseFinished {
            phase: wheat_git::AtomicImportPhase::ParentSync,
            ..
        })
    ));
}

#[tokio::test]
async fn atomic_import_handles_repeated_file_edits_beyond_delta_depth() {
    let git = repeated_edit_fixture(600);
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("atomic");

    let head = atomic_import(git.path(), &destination, "refs/heads/main")
        .await
        .unwrap();
    let objects = PackedObjectStore::open(destination.join("storage")).unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    let repository = reader.hydrate(envelope.state).await.unwrap();
    let value = repository
        .view(labels.labels["main"].view)
        .unwrap()
        .tree
        .get("file")
        .expect("final file is visible");
    let expected = (0..256)
        .map(|line| format!("line {line:04}: unchanged fixture content\n"))
        .collect::<String>();
    let expected = format!("0600{}", &expected[4..]);
    assert_eq!(content.get(value.content).unwrap(), expected.as_bytes());
}

#[tokio::test]
async fn atomic_import_retains_binary_content_above_the_guarded_text_limit() {
    let (git, expected) = large_binary_fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("atomic");

    let head = atomic_import(git.path(), &destination, "refs/heads/main")
        .await
        .unwrap();
    let objects = PackedObjectStore::open(destination.join("storage")).unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    let repository = reader.hydrate(envelope.state).await.unwrap();
    let value = repository
        .view(labels.labels["main"].view)
        .unwrap()
        .tree
        .get("model.bin")
        .expect("large file is visible");

    assert_eq!(content.get(value.content).unwrap(), expected);
}

#[tokio::test]
async fn atomic_import_rejects_a_file_above_the_in_memory_import_bound() {
    let git = oversized_binary_fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("atomic");

    let error = atomic_import(git.path(), &destination, "refs/heads/main")
        .await
        .unwrap_err();
    let AtomicImportError::Stage(wheat_git::StagedImportError::EditCapture { source, .. }) = error
    else {
        panic!("unexpected error: {error:?}")
    };
    assert!(matches!(
        source,
        wheat_core::edit_capture::EditCaptureError::InputTooLarge {
            side: wheat_core::edit_capture::SnapshotSide::After,
            actual: 67_108_865,
            limit: 67_108_864
        }
    ));
    assert!(!destination.exists());
}

#[tokio::test]
async fn atomic_import_removes_only_marker_authenticated_stale_stages_for_its_destination() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");
    let stale = private_stage(root.path(), "repository", "stale");
    let other_destination = private_stage(root.path(), "elsewhere", "stale");
    let similarly_named_destination = private_stage(root.path(), "repository-copy", "stale");
    let malformed = private_stage(root.path(), "repository", "malformed");
    let missing_marker = private_stage(root.path(), "repository", "missing-marker");
    let marker_directory = private_stage(root.path(), "repository", "marker-directory");
    let marker_symlink = private_stage(root.path(), "repository", "marker-symlink");
    let stage_symlink = private_stage(root.path(), "repository", "stage-symlink");
    let symlink_target = root.path().join("symlink-target");

    for path in [
        &stale,
        &other_destination,
        &similarly_named_destination,
        &malformed,
        &missing_marker,
        &marker_directory,
        &marker_symlink,
        &symlink_target,
    ] {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("sentinel"), b"keep").unwrap();
    }
    std::fs::write(stale.join("import.marker"), b"wheat-atomic-import-v1\n").unwrap();
    for path in [&other_destination, &similarly_named_destination] {
        std::fs::write(path.join("import.marker"), b"wheat-atomic-import-v1\n").unwrap();
    }
    std::fs::write(malformed.join("import.marker"), b"wrong\n").unwrap();
    std::fs::create_dir(marker_directory.join("import.marker")).unwrap();
    let external_marker = root.path().join("external-marker");
    std::fs::write(&external_marker, b"wheat-atomic-import-v1\n").unwrap();
    symlink(&external_marker, marker_symlink.join("import.marker")).unwrap();
    symlink(&symlink_target, &stage_symlink).unwrap();

    atomic_import(git.path(), &destination, "refs/heads/main")
        .await
        .unwrap();

    assert!(!stale.exists());
    for path in [
        &other_destination,
        &similarly_named_destination,
        &malformed,
        &missing_marker,
        &marker_directory,
        &marker_symlink,
        &symlink_target,
    ] {
        assert_eq!(std::fs::read(path.join("sentinel")).unwrap(), b"keep");
    }
    assert!(
        stage_symlink
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn destination_exists_does_not_clean_authenticated_stale_stages() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");
    let stale = private_stage(root.path(), "repository", "stale");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("sentinel"), b"keep").unwrap();
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("import.marker"), b"wheat-atomic-import-v1\n").unwrap();
    std::fs::write(stale.join("sentinel"), b"stale").unwrap();

    assert!(matches!(
        atomic_import(git.path(), &destination, "refs/heads/main").await,
        Err(AtomicImportError::DestinationExists(path)) if path == destination
    ));
    assert_eq!(
        std::fs::read(destination.join("sentinel")).unwrap(),
        b"keep"
    );
    assert_eq!(std::fs::read(stale.join("sentinel")).unwrap(), b"stale");
}

#[tokio::test]
async fn repeated_atomic_failures_do_not_accumulate_private_stages() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");
    let stale = private_stage(root.path(), "repository", "stale");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("import.marker"), b"wheat-atomic-import-v1\n").unwrap();

    for _ in 0..3 {
        assert!(
            atomic_import(git.path(), &destination, "refs/heads/missing")
                .await
                .is_err()
        );
        assert!(stage_names(root.path(), "repository").is_empty());
    }
}

#[tokio::test]
async fn atomic_finalization_failure_removes_its_private_stage_and_leaves_no_destination() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");

    let error = atomic_import(git.path(), &destination, "refs/heads/missing")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AtomicImportError::Finalize(wheat_git::FinalizeImportError::UnknownWorkingBranch(branch))
            if branch == "refs/heads/missing"
    ));
    assert!(!destination.exists());
    let leftovers = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("wheat-import-") && name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "private stages remain: {leftovers:?}");
}
