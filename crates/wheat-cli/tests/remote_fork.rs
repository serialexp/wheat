#![cfg(target_os = "linux")]

//! Forks served from a repositories directory: a fork shares its parent's network,
//! and content is kept once for every network.

mod remote_support;

use std::fs;
use std::path::{Path, PathBuf};

use remote_support::{ok, serve_repos_dir, write_config};
use wheat_store::RepositoriesDirectory;

/// Clones `url` into workspace `name` under `root`; returns it and its store home.
fn clone(root: &Path, config: &Path, url: &str, name: &str) -> (PathBuf, PathBuf) {
    let stores = root.join(format!("{name}-stores"));
    let workspace = root.join(name);
    ok(
        root,
        &stores,
        config,
        &["clone", url, workspace.to_str().unwrap()],
    );
    (workspace, stores)
}

/// Adds a change in `workspace` that writes `<name>.txt` and publishes it.
fn publish(config: &Path, (workspace, stores): &(PathBuf, PathBuf), name: &str) {
    ok(
        workspace,
        stores,
        config,
        &[
            "change",
            "new",
            "--path",
            &format!("{name}.txt"),
            "--value",
            name,
            "--message",
            name,
        ],
    );
    ok(workspace, stores, config, &["publish"]);
}

/// A fork made while its parent is served holds the parent's history and content,
/// and what is published to one is not the other's.
#[test]
fn a_fork_is_served_with_its_parents_history_and_content() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let repos = root.path().join("repos");
    fs::create_dir(&repos).unwrap();
    let repositories = RepositoriesDirectory::new(&repos);
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(repositories.create_empty("team/repo", "main"))
        .unwrap();
    let server = serve_repos_dir(&repos);
    let url = |name: &str| format!("wheat://{}/{name}", server.addr);

    let parent = clone(root.path(), &config, &url("team/repo"), "parent");
    publish(&config, &parent, "one");
    repositories.fork("team/repo", "fork/repo").unwrap();

    let fork = clone(root.path(), &config, &url("fork/repo"), "fork");
    assert_eq!(fs::read_to_string(fork.0.join("one.txt")).unwrap(), "one");
    publish(&config, &fork, "two");

    let reader = clone(root.path(), &config, &url("team/repo"), "reader");
    assert_eq!(fs::read_to_string(reader.0.join("one.txt")).unwrap(), "one");
    assert!(
        !reader.0.join("two.txt").exists(),
        "the fork's change is its own"
    );
    let again = clone(root.path(), &config, &url("fork/repo"), "again");
    assert_eq!(fs::read_to_string(again.0.join("two.txt")).unwrap(), "two");
}
