//! Serving the repositories in a directory.

use std::fs;

use wheat_server::registry::Registry;
use wheat_store::RepositoryDirectory;

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

    let registry = Registry::from_repos_dir(repos.path()).unwrap();

    assert_eq!(registry.names().collect::<Vec<_>>(), ["owner/repo"]);
    assert_eq!(
        registry
            .get("owner/repo")
            .unwrap()
            .heads
            .head()
            .await
            .unwrap(),
        head
    );
    drop(pending);
}
