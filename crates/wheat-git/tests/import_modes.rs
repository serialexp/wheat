use std::path::Path;

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
    let branches = atomic_reader
        .branch_manifest(atomic_envelope.branches)
        .await
        .unwrap();
    assert_eq!(branches.branches.len(), 2);
    assert_eq!(
        LocalPublicationStore::new(atomic.join("repository.head"))
            .read()
            .unwrap(),
        atomic_head
    );
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
    let repository = reader.hydrate(envelope.state).await.unwrap();
    let value = repository
        .working_view()
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
async fn atomic_import_never_replaces_an_existing_destination() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("sentinel"), b"keep").unwrap();

    assert!(matches!(
        atomic_import(git.path(), &destination, "refs/heads/main").await,
        Err(AtomicImportError::DestinationExists(path)) if path == destination
    ));
    assert_eq!(
        std::fs::read(destination.join("sentinel")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn atomic_failure_removes_its_private_stage_and_leaves_no_destination() {
    let git = fixture();
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("repository");

    assert!(
        atomic_import(git.path(), &destination, "refs/heads/missing")
            .await
            .is_err()
    );
    assert!(!destination.exists());
    let leftovers = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("wheat-import-") && name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "private stages remain: {leftovers:?}");
}
