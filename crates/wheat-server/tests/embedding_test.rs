//! Serving sessions on connections an embedder upgraded itself: a raw upgraded socket
//! through `serve_upgraded`, and the embedder's own WebSocket type through `serve`
//! and the `Transport` trait.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::http::{StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::Role;

use wheat_core::{ApplicationContext, Repository, Tree};
use wheat_protocol::*;
use wheat_server::connection::{Upgrade, serve, serve_upgraded};
use wheat_server::registry::Registry;
use wheat_server::server::ServerConfig;
use wheat_server::transport::{Transport, TransportError};
use wheat_store::{PackedObjectStore, RepositoryPublisher};

use support::{edit, fetch_complete, fetch_request};

/// A repository `test/repo` with one change, and its registry.
async fn registry(dir: &std::path::Path) -> Arc<Registry> {
    publish(dir).await;
    Arc::new(Registry::single_named("test/repo".to_owned(), dir).unwrap())
}

/// Publishes a repository with one change in `dir`, closing its store again.
async fn publish(dir: &std::path::Path) {
    let objects = PackedObjectStore::open(dir.join("storage")).unwrap();
    let content = objects.content_store();
    let heads = wheat_store::LocalPublicationStore::new(dir.join("repository.head"));
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
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
}

/// A WebSocket upgrade request for `path`, with the key from RFC 6455's example.
fn upgrade_request(path: &str) -> Request {
    Request::builder()
        .uri(path)
        .header(header::HOST, "wheat.test")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .unwrap()
}

fn client_hello() -> WheatProtocolMessage {
    WheatProtocolMessage::ClientHello(ClientHelloOutput {
        message_type: 0x02,
        selected_version: 1,
        receive_window_bytes: 16 * 1024 * 1024,
        receive_window_messages: 256,
        capabilities: vec![],
    })
}

async fn send_ws<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    message: &WheatProtocolMessage,
) {
    let bytes = encode_message(message).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn recv_ws<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> WheatProtocolMessage {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => return decode_message(&bytes).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_session_runs_on_a_socket_the_embedder_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(dir.path()).await;
    let config = Arc::new(ServerConfig::new());

    let upgrade = Upgrade::check(&upgrade_request("/test/repo"), &config).unwrap();
    assert_eq!(upgrade.repository(), "test/repo");
    let response = upgrade.response().unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    // RFC 6455's example answer to that key.
    assert_eq!(
        response.headers()[header::SEC_WEBSOCKET_ACCEPT],
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );

    // The embedder has answered 101; what is left is the upgraded socket.
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let server = tokio::spawn({
        let (registry, config) = (Arc::clone(&registry), Arc::clone(&config));
        async move { serve_upgraded(server_io, upgrade, &registry, &config, "embedded").await }
    });
    let mut ws = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;

    assert!(matches!(
        recv_ws(&mut ws).await,
        WheatProtocolMessage::ServerHello(_)
    ));
    send_ws(&mut ws, &client_hello()).await;
    match recv_ws(&mut ws).await {
        WheatProtocolMessage::SessionEstablished(se) => assert!(matches!(
            se.status,
            SessionEstablishedStatus::SessionStatusOk(_)
        )),
        other => panic!("expected SessionEstablished, got {other:?}"),
    }
    let head = match recv_ws(&mut ws).await {
        WheatProtocolMessage::RepositoryState(state) => support::envelope_id(&state.head_envelope),
        other => panic!("expected RepositoryState, got {other:?}"),
    };

    // A whole fetch runs over it.
    send_ws(&mut ws, &fetch_request(1, head, None)).await;
    let mut pushed = 0;
    loop {
        match recv_ws(&mut ws).await {
            WheatProtocolMessage::ObjectBatch(batch) => pushed += batch.objects.len() as u64,
            WheatProtocolMessage::FetchPushEnd(end) => {
                assert_eq!(end.pushed_objects, pushed);
                assert!(pushed >= 2);
                break;
            }
            other => panic!("expected ObjectBatch or FetchPushEnd, got {other:?}"),
        }
    }
    send_ws(&mut ws, &fetch_complete(1)).await;

    ws.close(None).await.unwrap();
    while let Some(Ok(_)) = ws.next().await {}
    server.await.unwrap();
}

/// An embedder's own WebSocket type, standing in for axum's: messages pass over
/// channels, and the session's end is recorded.
struct ChannelTransport {
    incoming: mpsc::Receiver<Bytes>,
    outgoing: mpsc::Sender<Bytes>,
    closed: Arc<AtomicBool>,
}

impl Transport for ChannelTransport {
    async fn send(&mut self, message: Bytes) -> Result<(), TransportError> {
        self.outgoing
            .send(message)
            .await
            .map_err(|_| TransportError::new("the client went away"))
    }

    async fn recv(&mut self) -> Result<Option<Bytes>, TransportError> {
        Ok(self.incoming.recv().await)
    }

    async fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// The client's ends of a [`ChannelTransport`].
struct ChannelClient {
    to_server: mpsc::Sender<Bytes>,
    from_server: mpsc::Receiver<Bytes>,
}

impl ChannelClient {
    async fn send(&self, message: &WheatProtocolMessage) {
        self.to_server
            .send(encode_message(message).unwrap().into())
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> WheatProtocolMessage {
        decode_message(&self.from_server.recv().await.unwrap()).unwrap()
    }
}

fn channel_transport() -> (ChannelTransport, ChannelClient, Arc<AtomicBool>) {
    let (to_server, incoming) = mpsc::channel(16);
    let (outgoing, from_server) = mpsc::channel(16);
    let closed = Arc::new(AtomicBool::new(false));
    let transport = ChannelTransport {
        incoming,
        outgoing,
        closed: Arc::clone(&closed),
    };
    let client = ChannelClient {
        to_server,
        from_server,
    };
    (transport, client, closed)
}

#[tokio::test]
async fn a_session_runs_on_the_embedders_own_transport() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(dir.path()).await;
    let config = Arc::new(ServerConfig::new());
    let upgrade = Upgrade::check(&upgrade_request("/test/repo"), &config).unwrap();
    let (transport, mut client, closed) = channel_transport();
    let server = tokio::spawn({
        let (registry, config) = (Arc::clone(&registry), Arc::clone(&config));
        async move { serve(transport, upgrade, &registry, &config, "embedded").await }
    });

    assert!(matches!(
        client.recv().await,
        WheatProtocolMessage::ServerHello(_)
    ));
    client.send(&client_hello()).await;
    assert!(matches!(
        client.recv().await,
        WheatProtocolMessage::SessionEstablished(_)
    ));
    let head = match client.recv().await {
        WheatProtocolMessage::RepositoryState(state) => support::envelope_id(&state.head_envelope),
        other => panic!("expected RepositoryState, got {other:?}"),
    };
    client.send(&fetch_request(1, head, None)).await;
    loop {
        match client.recv().await {
            WheatProtocolMessage::ObjectBatch(_) => {}
            WheatProtocolMessage::FetchPushEnd(_) => break,
            other => panic!("expected ObjectBatch or FetchPushEnd, got {other:?}"),
        }
    }
    client.send(&fetch_complete(1)).await;

    // The client goes away: the session ends and closes its transport.
    drop(client);
    server.await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_setup_deadline_holds_for_an_embedders_transport() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(dir.path()).await;
    let mut config = ServerConfig::new();
    config.session_setup_timeout = Duration::from_millis(100);
    let config = Arc::new(config);
    let upgrade = Upgrade::check(&upgrade_request("/test/repo"), &config).unwrap();
    let (transport, mut client, closed) = channel_transport();
    let server = tokio::spawn({
        let (registry, config) = (Arc::clone(&registry), Arc::clone(&config));
        async move { serve(transport, upgrade, &registry, &config, "embedded").await }
    });

    // The client never says hello.
    assert!(matches!(
        client.recv().await,
        WheatProtocolMessage::ServerHello(_)
    ));
    match client.recv().await {
        WheatProtocolMessage::Error(error) => assert_eq!(error.code, 408),
        other => panic!("expected the setup to time out, got {other:?}"),
    }
    server.await.unwrap();
    assert!(closed.load(Ordering::SeqCst));
}

#[test]
fn an_upgrade_is_checked_before_the_embedder_upgrades() {
    let mut config = ServerConfig::new();
    config.allowed_origins = vec!["https://wheat.test".to_owned()];

    let refusal = Upgrade::check(&upgrade_request("/"), &config)
        .err()
        .unwrap();
    assert_eq!(refusal.status, StatusCode::BAD_REQUEST);

    let mut foreign = upgrade_request("/test/repo");
    foreign
        .headers_mut()
        .insert(header::ORIGIN, "https://elsewhere.test".parse().unwrap());
    let refusal = Upgrade::check(&foreign, &config).err().unwrap();
    assert_eq!(refusal.status, StatusCode::FORBIDDEN);

    let mut allowed = upgrade_request("/test/repo");
    allowed
        .headers_mut()
        .insert(header::ORIGIN, "https://wheat.test".parse().unwrap());
    assert!(Upgrade::check(&allowed, &config).is_ok());

    // A request that is not a WebSocket upgrade has no 101 answer.
    let mut plain = upgrade_request("/test/repo");
    plain.headers_mut().remove(header::SEC_WEBSOCKET_KEY);
    let upgrade = Upgrade::check(&plain, &config).unwrap();
    assert_eq!(
        upgrade.response().unwrap_err().status,
        StatusCode::BAD_REQUEST
    );
}
