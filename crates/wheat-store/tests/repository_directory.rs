//! Creating and deleting a repository directory as a server serves it.

use std::{fs, path::Path};

use wheat_store::{
    LocalPublicationStore, PublicationStore, RepositoryDirectory, RepositoryDirectoryError,
    RepositoryReader,
};

/// The names in `parent` other than `keep`, sorted.
fn leftovers(parent: &Path, keep: &[&str]) -> Vec<String> {
    let mut names = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !keep.contains(&name.as_str()) && !name.ends_with(".wheat.lock"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn an_empty_repository_has_its_label_on_the_base_view() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));

    let head = directory.create_empty("main").await.unwrap();

    assert_eq!(head.generation, 0);
    assert_eq!(
        LocalPublicationStore::new(directory.head()).read().unwrap(),
        head
    );
    let objects = directory.open_store().unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    assert_eq!(labels.labels.keys().collect::<Vec<_>>(), ["main"]);
    // Only the repository and its name's lock are left beside it.
    assert_eq!(leftovers(parent.path(), &["repo"]), Vec::<String>::new());
}

#[tokio::test]
async fn creating_over_an_existing_directory_fails_and_leaves_it() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("keep"), b"keep").unwrap();

    let error = RepositoryDirectory::new(&root)
        .create_empty("main")
        .await
        .unwrap_err();

    assert!(matches!(error, RepositoryDirectoryError::Exists(path) if path == root));
    assert_eq!(fs::read(root.join("keep")).unwrap(), b"keep");
    assert_eq!(leftovers(parent.path(), &["repo"]), Vec::<String>::new());
}

#[tokio::test]
async fn an_invalid_label_creates_nothing() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));

    let error = directory.create_empty("").await.unwrap_err();

    assert!(matches!(error, RepositoryDirectoryError::Label(_)));
    assert_eq!(leftovers(parent.path(), &[]), Vec::<String>::new());
}

#[test]
fn a_dropped_stage_is_removed_and_nothing_is_installed() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));

    let pending = directory.begin_create().unwrap();
    let stage = pending.directory().root().to_path_buf();
    assert!(stage.is_dir());
    drop(pending);

    assert!(!stage.exists());
    assert!(!directory.root().exists());
}

#[test]
fn a_second_create_of_one_name_is_refused_while_the_first_runs() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));

    let _first = directory.begin_create().unwrap();

    assert!(matches!(
        directory.begin_create().unwrap_err(),
        RepositoryDirectoryError::Locked(_)
    ));
    assert!(matches!(
        directory.delete().unwrap_err(),
        RepositoryDirectoryError::Locked(_)
    ));
}

#[tokio::test]
async fn an_open_repository_is_not_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));
    directory.create_empty("main").await.unwrap();

    let store = directory.open_store().unwrap();
    let error = directory.delete().unwrap_err();

    assert!(matches!(error, RepositoryDirectoryError::InUse(_)));
    assert!(directory.head().is_file());
    store.close().unwrap();

    directory.delete().unwrap();
    assert!(!directory.root().exists());
    assert_eq!(leftovers(parent.path(), &[]), Vec::<String>::new());
}

#[tokio::test]
async fn a_deleted_name_can_be_created_again() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));
    let first = directory.create_empty("main").await.unwrap();
    directory.delete().unwrap();

    let second = directory.create_empty("trunk").await.unwrap();

    // The same empty repository under another label is another envelope.
    assert_ne!(first.envelope, second.envelope);
    assert_eq!(directory.publications().read().unwrap(), second);
}

#[test]
fn deleting_what_is_missing_or_not_a_repository_fails() {
    let parent = tempfile::tempdir().unwrap();
    let missing = RepositoryDirectory::new(parent.path().join("missing"));
    assert!(matches!(
        missing.delete().unwrap_err(),
        RepositoryDirectoryError::Missing(_)
    ));

    let other = parent.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("repository.head"), b"not really").unwrap();
    assert!(matches!(
        RepositoryDirectory::new(&other).delete().unwrap_err(),
        RepositoryDirectoryError::NotARepository(_)
    ));
    assert!(other.join("repository.head").is_file());
}

#[tokio::test]
async fn what_a_crash_left_is_swept_and_nothing_else() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));
    // A crashed create's stage and a crashed delete's tombstone, both marked.
    for crashed in [
        ".repo.wheat-stage-crashed.tmp",
        ".repo.wheat-deleted-crashed.tmp",
    ] {
        let path = parent.path().join(crashed);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("stage.marker"), b"wheat-repository-stage-v1\n").unwrap();
    }
    // Same shape, but unmarked, wrongly marked, or another name's.
    let unmarked = parent.path().join(".repo.wheat-stage-unmarked.tmp");
    fs::create_dir(&unmarked).unwrap();
    let wrong = parent.path().join(".repo.wheat-stage-wrong.tmp");
    fs::create_dir(&wrong).unwrap();
    fs::write(wrong.join("stage.marker"), b"something else\n").unwrap();
    let other = parent.path().join(".other.wheat-stage-crashed.tmp");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("stage.marker"), b"wheat-repository-stage-v1\n").unwrap();

    directory.create_empty("main").await.unwrap();

    assert_eq!(
        leftovers(parent.path(), &["repo"]),
        [
            ".other.wheat-stage-crashed.tmp",
            ".repo.wheat-stage-unmarked.tmp",
            ".repo.wheat-stage-wrong.tmp",
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_marker_reached_through_a_symlink_is_not_swept() {
    let parent = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(
        target.path().join("stage.marker"),
        b"wheat-repository-stage-v1\n",
    )
    .unwrap();
    let linked = parent.path().join(".repo.wheat-stage-linked.tmp");
    std::os::unix::fs::symlink(target.path(), &linked).unwrap();
    let marker_link = parent.path().join(".repo.wheat-stage-marker.tmp");
    fs::create_dir(&marker_link).unwrap();
    std::os::unix::fs::symlink(
        target.path().join("stage.marker"),
        marker_link.join("stage.marker"),
    )
    .unwrap();

    drop(
        RepositoryDirectory::new(parent.path().join("repo"))
            .begin_create()
            .unwrap(),
    );

    assert!(target.path().join("stage.marker").is_file());
    assert!(linked.exists());
    assert!(marker_link.is_dir());
}
