//! Serving the repositories in a directory, and repositories an opener opens when
//! asked for and the registry closes when idle.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use wheat_server::registry::{
    OpenError, Registry, RegistryLimits, RepositoryEntry, RepositoryOpener,
};
use wheat_store::{
    NetworkDirectory, RepositoriesDirectory, RepositoriesDirectoryError, RepositoryDirectoryError,
    RepositoryHead,
};

/// Limits that close every repository nothing holds whenever asked.
const CLOSE_AT_ONCE: RegistryLimits = RegistryLimits {
    idle: Duration::ZERO,
    max_open: 256,
};

/// Creates the empty repository `name` (`owner/repo`) in the repositories directory
/// `root`; returns its network's directory and its head.
async fn create(root: &Path, name: &str) -> (NetworkDirectory, RepositoryHead) {
    let repositories = RepositoriesDirectory::new(root);
    let placed = repositories.create_empty(name, "main").await.unwrap();
    (repositories.network(placed.location.network), placed.head)
}

fn in_use(error: RepositoriesDirectoryError) -> bool {
    matches!(
        error,
        RepositoriesDirectoryError::Directory(RepositoryDirectoryError::InUse(_))
    )
}

#[tokio::test]
async fn serves_created_repositories_and_skips_hidden_ones() {
    let repos = tempfile::tempdir().unwrap();
    let (_, head) = create(repos.path(), "owner/repo").await;
    // A hidden owner, and a name with no repository behind it.
    fs::create_dir_all(repos.path().join("names/.hidden")).unwrap();
    fs::copy(
        repos.path().join("names/owner/repo"),
        repos.path().join("names/.hidden/repo"),
    )
    .unwrap();
    fs::write(repos.path().join("names/owner/broken"), b"not a location").unwrap();

    let registry = Registry::from_repos_dir(repos.path(), RegistryLimits::default());

    assert!(
        registry.open_names().is_empty(),
        "nothing opens until asked"
    );
    let entry = registry.get("owner/repo").await.unwrap().unwrap();
    assert_eq!(entry.heads.head().await.unwrap(), head);
    assert_eq!(registry.open_names(), ["owner/repo"]);
    for missing in [".hidden/repo", "owner/absent", "owner", "a/b/c"] {
        assert!(
            registry.get(missing).await.unwrap().is_none(),
            "{missing} should not be found"
        );
    }
    assert!(
        registry.get("owner/broken").await.is_err(),
        "a damaged name is an error, not a missing repository"
    );
    assert_eq!(registry.open_names(), ["owner/repo"]);
}

#[tokio::test]
async fn a_repository_not_found_is_looked_up_again_next_time() {
    let repos = tempfile::tempdir().unwrap();
    let registry = Registry::from_repos_dir(repos.path(), RegistryLimits::default());
    assert!(registry.get("owner/later").await.unwrap().is_none());
    create(repos.path(), "owner/later").await;
    assert!(registry.get("owner/later").await.unwrap().is_some());
}

#[tokio::test]
async fn an_idle_repository_is_closed_only_once_nothing_holds_it() {
    let repos = tempfile::tempdir().unwrap();
    let (directory, head) = create(repos.path(), "owner/repo").await;
    let repositories = RepositoriesDirectory::new(repos.path());
    let registry = Registry::from_repos_dir(repos.path(), CLOSE_AT_ONCE);

    let entry = registry.get("owner/repo").await.unwrap().unwrap();
    assert_eq!(registry.close_idle(), 0, "a held repository stays open");
    assert!(
        in_use(repositories.delete("owner/repo").unwrap_err()),
        "the open store is locked"
    );
    drop(entry);
    assert_eq!(registry.close_idle(), 1);
    assert!(registry.open_names().is_empty());

    // Closed means its store is closed: this process can open it again.
    drop(directory.open_store().unwrap());
    let entry = registry.get("owner/repo").await.unwrap().unwrap();
    assert_eq!(entry.heads.head().await.unwrap(), head);
}

#[tokio::test]
async fn past_the_limit_the_least_recently_used_unheld_repository_is_closed() {
    let repos = tempfile::tempdir().unwrap();
    for name in ["owner/a", "owner/b", "owner/c"] {
        create(repos.path(), name).await;
    }
    let limits = RegistryLimits {
        idle: Duration::from_secs(3600),
        max_open: 2,
    };
    let registry = Registry::from_repos_dir(repos.path(), limits);
    let held = registry.get("owner/a").await.unwrap().unwrap();
    drop(registry.get("owner/b").await.unwrap().unwrap());
    drop(registry.get("owner/c").await.unwrap().unwrap());
    let mut open = registry.open_names();
    open.sort();
    assert_eq!(open, ["owner/a", "owner/c"], "b made room; a is held");

    // With every open one held, the registry opens past its limit rather than refuse.
    let c = registry.get("owner/c").await.unwrap().unwrap();
    let b = registry.get("owner/b").await.unwrap().unwrap();
    assert_eq!(registry.open_names().len(), 3);
    drop((held, b, c));
}

/// Opens an empty repository for any name, once let through, counting opens.
struct GatedOpener {
    root: tempfile::TempDir,
    opens: AtomicUsize,
    gate: Notify,
}

#[wheat_server::async_trait]
impl RepositoryOpener for GatedOpener {
    async fn open(&self, name: &str) -> Result<Option<RepositoryEntry>, OpenError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.gate.notified().await;
        let (directory, _) = create(self.root.path(), name).await;
        let repository = directory.repositories().unwrap()[0];
        Ok(Some(RepositoryEntry::new(
            Box::new(directory.open_store().unwrap()),
            directory.publications(repository),
        )))
    }
}

#[tokio::test]
async fn sessions_asking_at_once_open_a_repository_once() {
    let opener = Arc::new(GatedOpener {
        root: tempfile::tempdir().unwrap(),
        opens: AtomicUsize::new(0),
        gate: Notify::new(),
    });
    let registry = Arc::new(Registry::with_opener(
        opener.clone(),
        RegistryLimits::default(),
    ));
    let ask = |registry: Arc<Registry>| {
        tokio::spawn(async move { registry.get("owner/repo").await.unwrap().unwrap() })
    };
    let first = ask(registry.clone());
    let second = ask(registry.clone());
    // Let the one open through once both are waiting on it.
    while opener.opens.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    // Twice, so a second open, if there were one, would finish and be counted.
    opener.gate.notify_one();
    opener.gate.notify_one();
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(opener.opens.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn the_repositories_of_a_network_share_its_store_until_the_last_is_closed() {
    let repos = tempfile::tempdir().unwrap();
    let (network, head) = create(repos.path(), "alice/repo").await;
    let repositories = RepositoriesDirectory::new(repos.path());
    let fork = repositories.fork("alice/repo", "bob/repo").unwrap();
    let registry = Registry::from_repos_dir(repos.path(), CLOSE_AT_ONCE);

    // Both open at once: a second open of the packed store would be refused.
    let parent = registry.get("alice/repo").await.unwrap().unwrap();
    let child = registry.get("bob/repo").await.unwrap().unwrap();
    assert_eq!(parent.heads.head().await.unwrap(), head);
    assert_eq!(child.heads.head().await.unwrap(), fork.head);
    assert!(std::ptr::eq(&*parent.network.store, &*child.network.store));

    // Closing one leaves the store open for the other.
    drop(parent);
    assert_eq!(registry.close_idle(), 1);
    assert!(in_use(repositories.delete("alice/repo").unwrap_err()));
    let reopened = registry.get("alice/repo").await.unwrap().unwrap();
    assert!(std::ptr::eq(
        &*reopened.network.store,
        &*child.network.store
    ));

    // Closing the last closes the store.
    drop((reopened, child));
    assert_eq!(registry.close_idle(), 2);
    drop(network.open_store().unwrap());
    repositories.delete("bob/repo").unwrap();
    repositories.delete("alice/repo").unwrap();
}

#[tokio::test]
async fn quiescing_a_repository_holds_its_whole_network() {
    let repos = tempfile::tempdir().unwrap();
    create(repos.path(), "alice/repo").await;
    create(repos.path(), "carol/other").await;
    RepositoriesDirectory::new(repos.path())
        .fork("alice/repo", "bob/repo")
        .unwrap();
    let registry = Registry::from_repos_dir(repos.path(), RegistryLimits::default());

    let quiesced = registry.quiesce("alice/repo").await.unwrap().unwrap();

    let fork = registry.get("bob/repo").await.unwrap().unwrap();
    let other = registry.get("carol/other").await.unwrap().unwrap();
    assert!(fork.network.is_quiesced());
    assert!(!other.network.is_quiesced());
    drop(quiesced);
    assert!(!fork.network.is_quiesced());
}
