//! Deleting a repository while the server serves it: sessions using it end, the name
//! finds nothing, and the files go once nothing holds them open.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use support::{connect_and_handshake, open_repo, recv_msg};
use wheat_protocol::WheatProtocolMessage;
use wheat_server::registry::{DeleteError, Registry, RegistryLimits};
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::RepositoriesDirectory;

/// A directory with `alice/repo` and its fork `bob/repo`.
async fn forked() -> (tempfile::TempDir, RepositoriesDirectory) {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    repositories.fork("alice/repo", "bob/repo").unwrap();
    (root, repositories)
}

#[tokio::test]
async fn a_fork_is_deleted_while_its_parent_is_served() {
    let (root, repositories) = forked().await;
    let registry = Registry::from_repos_dir(root.path(), RegistryLimits::default()).unwrap();
    let parent = registry.get("alice/repo").await.unwrap().unwrap();
    let fork = registry.get("bob/repo").await.unwrap().unwrap();

    // The fork is held, as by a session; the network stays open for the parent.
    let deleting = registry.delete("bob/repo");
    let ended = async {
        fork.removed().await;
        drop(fork);
    };
    let (deleted, ()) = tokio::join!(deleting, ended);
    deleted.unwrap();

    assert!(registry.get("bob/repo").await.unwrap().is_none());
    assert!(repositories.locate("bob/repo").unwrap().is_none());
    assert!(registry.get("alice/repo").await.unwrap().is_some());
    drop(parent);
}

#[tokio::test]
async fn the_last_repository_goes_with_its_network_once_its_holders_let_go() {
    let root = tempfile::tempdir().unwrap();
    let repositories = RepositoriesDirectory::new(root.path());
    let placed = repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let network = repositories.network(placed.location.network);
    let registry = Registry::from_repos_dir(root.path(), RegistryLimits::default()).unwrap();
    let entry = registry.get("alice/repo").await.unwrap().unwrap();

    let deleting = registry.delete("alice/repo");
    let ended = async {
        entry.removed().await;
        // Held a while longer, as a session finishing what it was doing.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(network.root().exists(), "the store is still open");
        drop(entry);
    };
    let (deleted, ()) = tokio::join!(deleting, ended);
    deleted.unwrap();

    assert!(!network.root().exists());
    assert!(repositories.locate("alice/repo").unwrap().is_none());
    assert!(registry.get("alice/repo").await.unwrap().is_none());
}

#[tokio::test]
async fn deleting_what_is_not_there_or_not_served_by_a_directory_says_so() {
    let root = tempfile::tempdir().unwrap();
    let registry = Registry::from_repos_dir(root.path(), RegistryLimits::default()).unwrap();
    assert!(matches!(
        registry.delete("alice/missing").await,
        Err(DeleteError::Missing)
    ));
    // The name is free again afterwards.
    assert!(matches!(
        registry.delete("alice/missing").await,
        Err(DeleteError::Missing)
    ));

    let single = tempfile::tempdir().unwrap();
    let repository = wheat_store::RepositoryDirectory::new(single.path().join("repo"));
    repository.create_empty("main").await.unwrap();
    let registry = Registry::single_named("alice/repo".to_owned(), repository.root()).unwrap();
    assert!(matches!(
        registry.delete("alice/repo").await,
        Err(DeleteError::Unsupported)
    ));
}

#[tokio::test]
async fn a_session_ends_with_not_found_when_its_repository_is_deleted() {
    let (root, _) = forked().await;
    let registry =
        Arc::new(Registry::from_repos_dir(root.path(), RegistryLimits::default()).unwrap());
    let (_shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let served = Arc::clone(&registry);
    let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(any_port, served, ServerConfig::new(), ready, shutdown_rx)
            .await
            .unwrap();
    });
    let addr = ready_rx.await.unwrap();

    let mut ws = connect_and_handshake(addr, "bob/repo").await;
    open_repo(&mut ws).await;

    registry.delete("bob/repo").await.unwrap();

    match recv_msg(&mut ws).await {
        WheatProtocolMessage::Error(error) => {
            assert_eq!(error.code, 404);
            assert!(error.message.contains("was deleted"), "{}", error.message);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
