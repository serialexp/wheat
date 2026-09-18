//! End-to-end protocol tests: start a server in-process, connect with a
//! tokio-tungstenite client, and exercise the full protocol flow.

use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{
    ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    ObjectId, Repository, RevisionEdits, Tree,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    ContentStore, LocalPublicationStore, LooseObjectStore, RepositoryPublisher,
};

fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

fn stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

/// Start a server on a random port. Returns the bound address and a
/// shutdown sender.
async fn start_server(
    data_dir: &std::path::Path,
    repo_name: &str,
) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let registry = Registry::single_named(repo_name.to_string(), data_dir);
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

/// Connect to the server and perform the handshake. Returns the WebSocket
/// stream after SessionEstablished.
async fn connect_and_handshake(
    addr: SocketAddr,
) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://{addr}");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    // Receive ServerHello.
    let server_hello = recv_msg(&mut ws).await;
    assert!(
        matches!(server_hello, WheatProtocolMessage::ServerHello(_)),
        "expected ServerHello, got {server_hello:?}"
    );

    // Send ClientHello.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ClientHello(ClientHelloOutput {
            message_type: 0x02,
            selected_version: 1,
            auth_token: vec![],
            receive_window_bytes: 16 * 1024 * 1024,
            receive_window_messages: 256,
        }),
    )
    .await;

    // Receive SessionEstablished.
    let established = recv_msg(&mut ws).await;
    match &established {
        WheatProtocolMessage::SessionEstablished(se) => {
            assert!(matches!(
                se.status,
                SessionEstablishedStatus::SessionStatusOk(_)
            ));
        }
        other => panic!("expected SessionEstablished, got {other:?}"),
    }

    ws
}

async fn send_msg(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    msg: &WheatProtocolMessage,
) {
    let bytes = encode_message(msg).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn recv_msg(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> WheatProtocolMessage {
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        match msg {
            Message::Binary(bytes) => return decode_message(&bytes).unwrap(),
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await.unwrap();
            }
            Message::Pong(_) | Message::Frame(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
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

    // Fetch.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::FetchRequest(FetchRequestOutput {
            message_type: 0x20,
            op_id: 1,
            labels: vec!["main".to_string()],
            since_generation: 0,
            since_envelope: Digest256 { bytes: vec![0; 32] },
        }),
    )
    .await;

    // Should get an ObjectBatch back with the envelope and state objects.
    let batch = recv_msg(&mut ws).await;
    let objects = match &batch {
        WheatProtocolMessage::ObjectBatch(ob) => {
            assert_eq!(ob.op_id, 1);
            assert_eq!(ob.sequence, 0);
            &ob.objects
        }
        other => panic!("expected ObjectBatch, got {other:?}"),
    };

    assert!(
        objects.len() >= 2,
        "first batch should contain at least envelope + state (got {})",
        objects.len()
    );

    // Verify the envelope object's identity matches the head.
    let envelope_obj = &objects[0];
    assert_eq!(envelope_obj.id.bytes, head_envelope.bytes);
    let actual_id = ObjectId::of_canonical_bytes(&envelope_obj.data);
    let claimed_id = ObjectId::from_digest(
        envelope_obj.id.bytes.as_slice().try_into().unwrap(),
    );
    assert_eq!(actual_id, claimed_id, "object identity must match");

    // Send FetchComplete.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
            message_type: 0x23,
            op_id: 1,
        }),
    )
    .await;

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
