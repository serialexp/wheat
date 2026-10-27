//! A server's repositories in fork networks: creating, forking, deleting and copying.

use std::{fs, path::Path};

use wheat_store::{
    NetworkDirectory, PublicationStore, RepositoriesDirectory, RepositoriesDirectoryError,
    RepositoryDirectoryError, RepositoryReader,
};

/// The names in `directory`, sorted, without name locks.
fn names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.ends_with(".wheat.lock"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

async fn labels(network: &NetworkDirectory, head: wheat_store::RepositoryHead) -> Vec<String> {
    let objects = network.open_store().unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let manifest = reader.view_label_manifest(envelope.labels).await.unwrap();
    let labels = manifest.labels.keys().cloned().collect();
    drop(content);
    objects.close().unwrap();
    labels
}

#[tokio::test]
async fn a_new_repository_has_a_network_of_its_own() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());

    let first = repositories
        .create_empty("alice/one", "main")
        .await
        .unwrap();
    let second = repositories
        .create_empty("alice/two", "main")
        .await
        .unwrap();

    assert_ne!(first.location.network, second.location.network);
    assert_eq!(
        repositories.locate("alice/one").unwrap(),
        Some(first.location)
    );
    let network = repositories.network(first.location.network);
    assert_eq!(network.repositories().unwrap(), [first.location.repository]);
    assert_eq!(
        network
            .publications(first.location.repository)
            .read()
            .unwrap(),
        first.head
    );
    assert_eq!(labels(&network, first.head).await, ["main"]);
    // Only the names are left beside the names: no stage, no half-written record.
    assert_eq!(names(&root.path().join("names/alice")), ["one", "two"]);
    assert_eq!(names(&root.path().join("networks")).len(), 2);
}

#[tokio::test]
async fn a_fork_is_a_head_in_its_parents_network() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let parent = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let network = repositories.network(parent.location.network);
    let store_files = network
        .files()
        .unwrap()
        .into_iter()
        .filter(|file| file.path.starts_with("storage"))
        .collect::<Vec<_>>();

    let fork = repositories.fork("alice/repo", "bob/repo").unwrap();
    let fork_of_fork = repositories.fork("bob/repo", "carol/repo").unwrap();

    for child in [fork, fork_of_fork] {
        assert_eq!(child.location.network, parent.location.network);
        assert_eq!(child.head.generation, 0);
        assert_eq!(child.head.envelope, parent.head.envelope);
    }
    assert_eq!(
        repositories.locate("carol/repo").unwrap(),
        Some(fork_of_fork.location)
    );
    let mut heads = vec![
        parent.location.repository,
        fork.location.repository,
        fork_of_fork.location.repository,
    ];
    heads.sort_unstable();
    assert_eq!(network.repositories().unwrap(), heads);
    // Forking wrote nothing to the store.
    let after = network
        .files()
        .unwrap()
        .into_iter()
        .filter(|file| file.path.starts_with("storage"))
        .collect::<Vec<_>>();
    assert_eq!(after, store_files);
    assert_eq!(labels(&network, fork.head).await, ["main"]);
}

#[tokio::test]
async fn forking_refuses_a_missing_parent_and_a_taken_name() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    repositories.create_empty("bob/repo", "main").await.unwrap();

    assert!(matches!(
        repositories.fork("nobody/repo", "carol/repo").unwrap_err(),
        RepositoriesDirectoryError::Missing(name) if name == "nobody/repo"
    ));
    assert!(matches!(
        repositories.fork("alice/nothing", "carol/repo").unwrap_err(),
        RepositoriesDirectoryError::Missing(name) if name == "alice/nothing"
    ));
    assert!(matches!(
        repositories.fork("alice/repo", "bob/repo").unwrap_err(),
        RepositoriesDirectoryError::Exists(name) if name == "bob/repo"
    ));
    assert!(matches!(
        repositories.fork("alice/repo", "alice/repo").unwrap_err(),
        RepositoriesDirectoryError::Exists(_)
    ));
    assert!(repositories.locate("carol/repo").unwrap().is_none());
}

#[tokio::test]
async fn an_invalid_label_creates_nothing() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());

    let error = repositories
        .create_empty("alice/repo", "")
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RepositoriesDirectoryError::Directory(RepositoryDirectoryError::Label(_))
    ));
    assert_eq!(
        names(&root.path().join("names/alice")),
        Vec::<String>::new()
    );
    assert_eq!(names(&root.path().join("networks")), Vec::<String>::new());
}

#[tokio::test]
async fn names_are_owner_and_repository() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());

    for name in [
        "repo",
        "a/b/c",
        "/repo",
        "alice/",
        ".alice/repo",
        "alice/.repo",
    ] {
        assert!(
            matches!(
                repositories.create_empty(name, "main").await.unwrap_err(),
                RepositoriesDirectoryError::InvalidName(_)
            ),
            "{name}"
        );
    }
    assert!(!root.path().join("names").exists());
}

#[tokio::test]
async fn deleting_a_fork_keeps_its_network_and_deleting_the_last_removes_it() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let parent = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let fork = repositories.fork("alice/repo", "bob/repo").unwrap();
    let network = repositories.network(parent.location.network);

    repositories.delete("alice/repo").unwrap();

    assert!(repositories.locate("alice/repo").unwrap().is_none());
    assert_eq!(network.repositories().unwrap(), [fork.location.repository]);
    assert_eq!(
        repositories.locate("bob/repo").unwrap(),
        Some(fork.location)
    );
    assert_eq!(labels(&network, fork.head).await, ["main"]);

    repositories.delete("bob/repo").unwrap();

    assert!(!network.root().exists());
    assert_eq!(
        names(&root.path().join("names/alice")),
        Vec::<String>::new()
    );
    assert_eq!(names(&root.path().join("names/bob")), Vec::<String>::new());
    assert_eq!(names(&root.path().join("networks")), Vec::<String>::new());
    assert!(matches!(
        repositories.delete("bob/repo").unwrap_err(),
        RepositoriesDirectoryError::Missing(_)
    ));
}

#[tokio::test]
async fn a_repository_whose_network_is_open_is_not_deleted() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let parent = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    repositories.fork("alice/repo", "bob/repo").unwrap();
    let network = repositories.network(parent.location.network);

    let store = network.open_store().unwrap();
    for name in ["alice/repo", "bob/repo"] {
        assert!(matches!(
            repositories.delete(name).unwrap_err(),
            RepositoriesDirectoryError::Directory(RepositoryDirectoryError::InUse(_))
        ));
    }
    store.close().unwrap();

    repositories.delete("bob/repo").unwrap();
    repositories.delete("alice/repo").unwrap();
    assert!(!network.root().exists());
}

#[tokio::test]
async fn a_name_whose_head_is_gone_is_no_repository_and_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let first = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    // As a crash between moving the network away and removing the name leaves it.
    fs::remove_dir_all(repositories.network(first.location.network).root()).unwrap();

    assert!(repositories.locate("alice/repo").unwrap().is_none());
    let second = repositories
        .create_empty("alice/repo", "trunk")
        .await
        .unwrap();

    assert_eq!(
        repositories.locate("alice/repo").unwrap(),
        Some(second.location)
    );
    // And deleting such a name removes it.
    fs::remove_dir_all(repositories.network(second.location.network).root()).unwrap();
    repositories.delete("alice/repo").unwrap();
    assert_eq!(
        names(&root.path().join("names/alice")),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn what_a_crash_left_beside_a_name_is_swept() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let parent = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let owner = root.path().join("names/alice");
    // A tombstone (marked, as only this module makes them) and a half-written name.
    let tombstone = owner.join(".repo.wheat-deleted-1.tmp");
    fs::create_dir(&tombstone).unwrap();
    fs::write(
        tombstone.join("stage.marker"),
        b"wheat-repository-stage-v1\n",
    )
    .unwrap();
    fs::write(owner.join(".repo.wheat-name-2.tmp"), b"half").unwrap();
    // Not ours: kept.
    fs::write(owner.join(".repo.wheat-other.tmp"), b"keep").unwrap();

    repositories.delete("alice/repo").unwrap();

    assert!(
        !repositories
            .network(parent.location.network)
            .root()
            .exists()
    );
    assert_eq!(names(&owner), [".repo.wheat-other.tmp"]);
}

#[tokio::test]
async fn a_network_copies_with_every_head() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let parent = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let fork = repositories.fork("alice/repo", "bob/repo").unwrap();
    let network = repositories.network(parent.location.network);
    let copy = NetworkDirectory::new(root.path().join("copy"));

    let files = network.copy_to(&copy).unwrap();

    assert_eq!(copy.files().unwrap(), files);
    for placed in [parent, fork] {
        let path = format!(
            "heads/{}/repository.head",
            placed.location.repository.as_simple()
        );
        assert!(files.iter().any(|file| file.path == Path::new(&path)));
        assert_eq!(
            copy.publications(placed.location.repository)
                .read()
                .unwrap(),
            placed.head
        );
    }
    assert_eq!(labels(&copy, fork.head).await, ["main"]);
}
