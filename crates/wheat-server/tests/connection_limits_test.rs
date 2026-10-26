//! The limits that stop connections nobody has authenticated from piling up: the
//! session-setup deadline, and the cap on connections open at once.

use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async, connect_async};

use wheat_protocol::*;
use wheat_server::access::{Access, AccessError, AccessProvider, Principal, UpgradeRequest};
use wheat_server::config::{FileConfig, LimitsConfig};
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};

type Ws<S> = WebSocketStream<S>;

const REPO: &str = "team/repo";
/// The setup deadline in tests that wait for it to pass.
const SHORT_SETUP: Duration = Duration::from_millis(200);
/// A setup deadline no test reaches.
const LONG_SETUP: Duration = Duration::from_secs(3600);
/// How long a test waits for the server to do something it should do well before.
const PATIENCE: Duration = Duration::from_secs(30);

struct Server {
    addr: SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

/// Start a server on a random port. The repository is empty: these tests never get
/// past setting up the session.
async fn start(config: ServerConfig) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::single_named(REPO.to_owned(), dir.path()).unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(addr, registry, config, ready_tx, shutdown_rx)
            .await
            .unwrap();
    });
    Server {
        addr: ready_rx.await.unwrap(),
        _shutdown: shutdown,
        _dir: dir,
    }
}

fn limits(session_setup_timeout: Duration, max_connections: usize) -> ServerConfig {
    let mut config = ServerConfig::new();
    config.session_setup_timeout = session_setup_timeout;
    config.max_connections = max_connections;
    config
}

fn url(addr: SocketAddr) -> String {
    format!("ws://{addr}/{REPO}")
}

/// Wait for the server to close `stream`, dropping anything it sends first.
async fn closed_by_server(stream: &mut TcpStream) {
    let mut buffer = [0; 1024];
    let closed = async {
        while let Ok(read) = stream.read(&mut buffer).await {
            if read == 0 {
                return;
            }
        }
    };
    tokio::time::timeout(PATIENCE, closed)
        .await
        .expect("the server did not close the connection");
}

/// The server's next protocol message, or `None` once it closes the WebSocket.
async fn next_message<S>(ws: &mut Ws<S>) -> Option<WheatProtocolMessage>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let next = async {
        while let Some(message) = ws.next().await {
            match message {
                Ok(Message::Binary(bytes)) => return Some(decode_message(&bytes).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
        None
    };
    tokio::time::timeout(PATIENCE, next)
        .await
        .expect("the server neither answered nor closed")
}

async fn expect_server_hello<S>(ws: &mut Ws<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match next_message(ws).await {
        Some(WheatProtocolMessage::ServerHello(_)) => {}
        other => panic!("expected ServerHello, got {other:?}"),
    }
}

/// The session ends with the error for a setup that took too long, then closes.
async fn expect_setup_timed_out(ws: &mut Ws<MaybeTlsStream<TcpStream>>) {
    match next_message(ws).await {
        Some(WheatProtocolMessage::Error(error)) => {
            assert_eq!(error.code, 408, "{}", error.message);
        }
        other => panic!("expected a 408 error, got {other:?}"),
    }
    assert!(
        next_message(ws).await.is_none(),
        "the session was not closed"
    );
}

#[tokio::test]
async fn a_connection_that_never_upgrades_is_closed() {
    let server = start(limits(SHORT_SETUP, 16)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    closed_by_server(&mut stream).await;
}

#[tokio::test]
async fn a_session_that_never_says_hello_is_closed() {
    let server = start(limits(SHORT_SETUP, 16)).await;
    let (mut ws, _) = connect_async(url(server.addr)).await.unwrap();
    expect_server_hello(&mut ws).await;
    expect_setup_timed_out(&mut ws).await;
}

/// A provider that never says who is calling, like one whose database hangs.
struct Stalled;

#[async_trait]
impl AccessProvider for Stalled {
    async fn authenticate(
        &self,
        _request: &UpgradeRequest,
    ) -> Result<Option<Principal>, AccessError> {
        std::future::pending().await
    }

    async fn access(
        &self,
        _principal: &Principal,
        _repository: &str,
    ) -> Result<Option<Access>, AccessError> {
        Ok(Some(Access::Force))
    }
}

#[tokio::test]
async fn an_access_provider_that_never_answers_does_not_hold_the_session() {
    let mut config = limits(SHORT_SETUP, 16);
    config.access = Arc::new(Stalled);
    let server = start(config).await;
    let (mut ws, _) = connect_async(url(server.addr)).await.unwrap();
    expect_server_hello(&mut ws).await;
    let hello = WheatProtocolMessage::ClientHello(ClientHelloOutput {
        message_type: 0x02,
        selected_version: 1,
        receive_window_bytes: 16 * 1024 * 1024,
        receive_window_messages: 256,
        capabilities: vec![],
    });
    ws.send(Message::Binary(encode_message(&hello).unwrap().into()))
        .await
        .unwrap();
    expect_setup_timed_out(&mut ws).await;
}

#[tokio::test]
async fn connections_past_the_limit_are_closed_until_one_ends() {
    let server = start(limits(LONG_SETUP, 1)).await;
    let held = TcpStream::connect(server.addr).await.unwrap();
    let mut turned_away = TcpStream::connect(server.addr).await.unwrap();
    closed_by_server(&mut turned_away).await;

    // The connection that got the one slot is still served.
    let (mut held, _) = client_async(url(server.addr), held).await.unwrap();
    expect_server_hello(&mut held).await;

    // Once it ends, its slot serves the next connection.
    drop(held);
    let served = async {
        loop {
            if let Ok((mut ws, _)) = connect_async(url(server.addr)).await
                && let Some(WheatProtocolMessage::ServerHello(_)) = next_message(&mut ws).await
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, served)
        .await
        .expect("no connection was served after the held one ended");
}

#[test]
fn the_config_file_sets_the_limits() {
    let defaults = ServerConfig::from_file(&FileConfig::default()).unwrap();
    assert_eq!(
        defaults.session_setup_timeout,
        wheat_server::server::DEFAULT_SESSION_SETUP_TIMEOUT
    );
    assert_eq!(
        defaults.max_connections,
        wheat_server::server::DEFAULT_MAX_CONNECTIONS
    );

    let config = ServerConfig::from_file(&FileConfig {
        limits: LimitsConfig {
            session_setup_seconds: NonZeroU64::new(3),
            max_connections: NonZeroUsize::new(64),
        },
        ..FileConfig::default()
    })
    .unwrap();
    assert_eq!(config.session_setup_timeout, Duration::from_secs(3));
    assert_eq!(config.max_connections, 64);
}
