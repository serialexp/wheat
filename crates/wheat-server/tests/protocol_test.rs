//! End-to-end protocol tests: start a server in-process, connect with a
//! tokio-tungstenite client, and exercise the full protocol flow.

mod support;

use std::net::SocketAddr;

use wheat_core::{ApplicationContext, ObjectId, Repository, Tree};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{ContentStore, LocalPublicationStore, PackedObjectStore, RepositoryPublisher};

use support::{connect_and_handshake, edit, fetch_complete, fetch_request, recv_msg, send_msg};

fn stores(root: &std::path::Path) -> (PackedObjectStore, ContentStore, LocalPublicationStore) {
    let objects = PackedObjectStore::open(root.join("storage")).unwrap();
    let content = objects.content_store();
    (
        objects,
        content,
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

/// Start a server on a random port. Returns the bound address and a
/// shutdown sender.
async fn start_server(
    data_dir: &std::path::Path,
    repo_name: &str,
) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let registry = Registry::single_named(repo_name.to_string(), data_dir).unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    tokio::spawn(async move {
        run_until(addr, registry, ServerConfig::new(), ready_tx, shutdown_rx)
            .await
            .unwrap();
    });

    let bound = ready_rx.await.unwrap();
    (bound, shutdown_tx)
}

/// Build a repository with one change, publish it locally. Returns the
/// Repository and the data dir path.
async fn setup_repo(dir: &std::path::Path) -> Repository {
    let (objects, content, heads) = stores(dir);
    content.put(b"hello wheat").unwrap();

    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;

    repo.create_change(
        basis,
        basis,
        [],
        edit("greeting.txt", b"hello wheat"),
        ApplicationContext::new(&content),
    )
    .unwrap();

    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    publisher.initialize(&repo).await.unwrap();

    repo
}

#[tokio::test]
async fn session_setup_and_repo_open() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(dir.path()).await;

    let (addr, shutdown) = start_server(dir.path(), "test/repo").await;
    let mut ws = connect_and_handshake(addr).await;

    // Open repository.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: "test/repo".to_string(),
        }),
    )
    .await;

    let state = recv_msg(&mut ws).await;
    match state {
        WheatProtocolMessage::RepositoryState(rs) => {
            // initialize() sets generation = 0 with an empty label manifest.
            assert_eq!(rs.head_generation, 0);
            assert!(!rs.labels_truncated);
        }
        other => panic!("expected RepositoryState, got {other:?}"),
    }

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn fetch_objects() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(dir.path()).await;

    let (addr, shutdown) = start_server(dir.path(), "test/repo").await;
    let mut ws = connect_and_handshake(addr).await;

    // Open.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: "test/repo".to_string(),
        }),
    )
    .await;
    let state = recv_msg(&mut ws).await;
    let head_envelope = match &state {
        WheatProtocolMessage::RepositoryState(rs) => rs.head_envelope.clone(),
        other => panic!("expected RepositoryState, got {other:?}"),
    };

    // Fetch with no `since`: the server pushes the whole closure, then says how
    // much it pushed.
    send_msg(
        &mut ws,
        &fetch_request(1, support::envelope_id(&head_envelope), None),
    )
    .await;
    let mut all_objects = Vec::new();
    loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ObjectBatch(ob) => {
                assert_eq!(ob.op_id, 1);
                all_objects.extend(ob.objects);
            }
            WheatProtocolMessage::FetchPushEnd(end) => {
                assert_eq!(end.op_id, 1);
                assert_eq!(end.pushed_objects, all_objects.len() as u64);
                assert!(!end.incremental);
                break;
            }
            other => panic!("expected ObjectBatch or FetchPushEnd, got {other:?}"),
        }
    }

    assert!(
        all_objects.len() >= 2,
        "should receive at least envelope + state (got {})",
        all_objects.len()
    );

    // Verify the envelope object is in the batch (not necessarily first).
    let envelope_obj = all_objects
        .iter()
        .find(|o| o.id.bytes == head_envelope.bytes)
        .expect("envelope object must be in the batch");
    let actual_id = ObjectId::of_canonical_bytes(&envelope_obj.data);
    let claimed_id = ObjectId::from_digest(envelope_obj.id.bytes.as_slice().try_into().unwrap());
    assert_eq!(actual_id, claimed_id, "object identity must match");

    send_msg(&mut ws, &fetch_complete(1)).await;

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn repo_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(dir.path()).await;

    let (addr, shutdown) = start_server(dir.path(), "test/repo").await;
    let mut ws = connect_and_handshake(addr).await;

    // Try to open a non-existent repository.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: "nonexistent/repo".to_string(),
        }),
    )
    .await;

    let error = recv_msg(&mut ws).await;
    match error {
        WheatProtocolMessage::Error(e) => {
            assert_eq!(e.code, 404);
            assert!(e.message.contains("not found"));
        }
        other => panic!("expected Error, got {other:?}"),
    }

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}
