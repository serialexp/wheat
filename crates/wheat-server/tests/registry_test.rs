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
use wheat_store::{RepositoryDirectory, RepositoryHead};

/// Limits that close every repository nothing holds whenever asked.
const CLOSE_AT_ONCE: RegistryLimits = RegistryLimits {
    idle: Duration::ZERO,
    max_open: 256,
};

/// Creates the empty repository `name` (`owner/repo`) under `root`.
async fn create(root: &Path, name: &str) -> (RepositoryDirectory, RepositoryHead) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let directory = RepositoryDirectory::new(path);
    let head = directory.create_empty("main").await.unwrap();
    (directory, head)
}

#[tokio::test]
async fn serves_created_repositories_and_skips_hidden_ones() {
    let repos = tempfile::tempdir().unwrap();
    let owner = repos.path().join("owner");
    fs::create_dir(&owner).unwrap();
    let directory = RepositoryDirectory::new(owner.join("repo"));
    let head = directory.create_empty("main").await.unwrap();
    // A create still in progress, and a hidden owner.
    let pending = RepositoryDirectory::new(owner.join("next"))
        .begin_create()
        .unwrap();
    fs::create_dir_all(repos.path().join(".hidden").join("repo")).unwrap();

    let registry = Registry::from_repos_dir(repos.path(), RegistryLimits::default());

    assert!(
        registry.open_names().is_empty(),
        "nothing opens until asked"
    );
    let entry = registry.get("owner/repo").await.unwrap().unwrap();
    assert_eq!(entry.heads.head().await.unwrap(), head);
    assert_eq!(registry.open_names(), ["owner/repo"]);
    for missing in [
        "owner/next",
        ".hidden/repo",
        "owner/absent",
        "owner",
        "a/b/c",
    ] {
        assert!(
            registry.get(missing).await.unwrap().is_none(),
            "{missing} should not be found"
        );
    }
    assert_eq!(registry.open_names(), ["owner/repo"]);
    drop(pending);
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
    let registry = Registry::from_repos_dir(repos.path(), CLOSE_AT_ONCE);

    let entry = registry.get("owner/repo").await.unwrap().unwrap();
    assert_eq!(registry.close_idle(), 0, "a held repository stays open");
    assert!(directory.delete().is_err(), "the open store is locked");
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
        Ok(Some(RepositoryEntry::open(directory.root()).unwrap()))
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
