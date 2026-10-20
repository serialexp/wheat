//! Authentication and per-repository access over the real protocol: start a
//! server in-process with a token file or a custom provider, and check what each
//! principal may do.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{
    ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    Repository, RevisionEdits, Tree,
};
use wheat_protocol::*;
use wheat_server::access::{
    Access, AccessError, AccessProvider, AuthConfig, Principal, PrincipalConfig, token_hash_hex,
};
use wheat_server::config::FileConfig;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{PackedObjectStore, RepositoryPublisher};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const REPO: &str = "team/repo";

async fn setup_repo(dir: &std::path::Path) {
    let objects = PackedObjectStore::open(dir.join("storage")).unwrap();
    let content = objects.content_store();
    let heads = wheat_store::LocalPublicationStore::new(dir.join("repository.head"));
    content.put(b"hello wheat").unwrap();
    let record = EditRecord::new(
        "greeting.txt",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(b"hello wheat"),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    let edits = RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    };
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    repo.create_change(basis, basis, [], edits, ApplicationContext::new(&content))
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
}

/// Principals used by the token-file tests: `reader` may read, `writer` may
/// publish, `admin` may force, `stranger` has a token but no repository rules.
fn token_file() -> ServerConfig {
    fn principal(name: &str, access: Option<Access>) -> PrincipalConfig {
        PrincipalConfig {
            name: name.to_owned(),
            token_hashes: vec![token_hash_hex(format!("{name}-token").as_bytes())],
            repositories: access
                .map(|access| [(REPO.to_owned(), access)].into())
                .unwrap_or_default(),
        }
    }
    ServerConfig::from_file(&FileConfig {
        auth: Some(AuthConfig {
            allowed_origins: vec!["https://allowed.example".to_owned()],
            principals: vec![
                principal("reader", Some(Access::Read)),
                principal("writer", Some(Access::Publish)),
                principal("admin", Some(Access::Force)),
                principal("stranger", None),
            ],
        }),
    })
    .unwrap()
}

struct Server {
    addr: SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

async fn start(config: ServerConfig) -> Server {
    let dir = tempfile::tempdir().unwrap();
    setup_repo(dir.path()).await;
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

async fn send_msg(ws: &mut Ws, msg: &WheatProtocolMessage) {
    let bytes = encode_message(msg).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn recv_msg(ws: &mut Ws) -> WheatProtocolMessage {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => return decode_message(&bytes).unwrap(),
            Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
            Message::Pong(_) | Message::Frame(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

/// Connect, send `token`, and return the socket with the SessionEstablished reply.
async fn hello(addr: SocketAddr, token: &[u8]) -> (Ws, SessionEstablishedOutput) {
    let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
    assert!(matches!(
        recv_msg(&mut ws).await,
        WheatProtocolMessage::ServerHello(_)
    ));
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ClientHello(ClientHelloOutput {
            message_type: 0x02,
            selected_version: 1,
            auth_token: token.to_vec(),
            receive_window_bytes: 16 * 1024 * 1024,
            receive_window_messages: 256,
        }),
    )
    .await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::SessionEstablished(se) => (ws, se),
        other => panic!("expected SessionEstablished, got {other:?}"),
    }
}

/// Log in with `token` and open `repo`. Returns the socket and the reply.
async fn open(addr: SocketAddr, token: &[u8], repo: &str) -> (Ws, WheatProtocolMessage) {
    let (mut ws, se) = hello(addr, token).await;
    assert!(
        matches!(se.status, SessionEstablishedStatus::SessionStatusOk(_)),
        "login failed: {}",
        se.message
    );
    send_msg(
        &mut ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: repo.to_owned(),
        }),
    )
    .await;
    let reply = recv_msg(&mut ws).await;
    (ws, reply)
}

async fn open_ok(addr: SocketAddr, token: &[u8]) -> (Ws, RepositoryStateOutput) {
    match open(addr, token, REPO).await {
        (ws, WheatProtocolMessage::RepositoryState(state)) => (ws, state),
        (_, other) => panic!("expected RepositoryState, got {other:?}"),
    }
}

/// Send a publish whose head check would fail. A principal allowed to publish
/// gets `stale`; one that is not gets `rejected` before the head is looked at.
/// A publish of the head itself that names a generation the head is not at, so a
/// forced publish is sent back as stale once it passes the access check.
fn publish_request(state: &RepositoryStateOutput, force: bool) -> WheatProtocolMessage {
    WheatProtocolMessage::PublishRequest(PublishRequestOutput {
        message_type: 0x40,
        op_id: 7,
        publish_key: Uuid128 { bytes: vec![1; 16] },
        envelope_id: state.head_envelope.clone(),
        expected_generation: state.head_generation + 99,
        expected_envelope: state.head_envelope.clone(),
        force,
    })
}

/// Sends [`publish_request`] and an empty push.
async fn send_publish(ws: &mut Ws, state: &RepositoryStateOutput, force: bool) {
    send_msg(ws, &publish_request(state, force)).await;
    send_msg(
        ws,
        &WheatProtocolMessage::PublishPushEnd(PublishPushEndOutput {
            message_type: 0x4b,
            op_id: 7,
            pushed_objects: 0,
            pushed_contents: 0,
        }),
    )
    .await;
}

async fn publish_reply(
    ws: &mut Ws,
    state: &RepositoryStateOutput,
    force: bool,
) -> PublishResultOutput {
    send_publish(ws, state, force).await;
    match recv_msg(ws).await {
        WheatProtocolMessage::PublishResult(result) => {
            assert_eq!(result.op_id, 7);
            result
        }
        other => panic!("expected PublishResult, got {other:?}"),
    }
}

fn is_permission_denial(result: &PublishResultOutput) -> bool {
    matches!(result.status, PublishResultStatus::PublishStatusRejected(_))
        && result.message.contains("permission denied")
}

fn fetch_request(op_id: u32, state: &RepositoryStateOutput) -> WheatProtocolMessage {
    WheatProtocolMessage::FetchRequest(FetchRequestOutput {
        message_type: 0x20,
        op_id,
        want_envelope: state.head_envelope.clone(),
        since_envelope: Digest256 { bytes: vec![0; 32] },
    })
}

#[tokio::test]
async fn unknown_empty_and_oversized_tokens_are_refused() {
    let server = start(token_file()).await;
    for token in [
        b"not-a-token".to_vec(),
        Vec::new(),
        vec![b'x'; wheat_server::access::MAX_AUTH_TOKEN_BYTES + 1],
    ] {
        let (_ws, se) = hello(server.addr, &token).await;
        assert!(
            matches!(
                se.status,
                SessionEstablishedStatus::SessionStatusAuthFailed(_)
            ),
            "token of {} bytes was accepted",
            token.len()
        );
    }
}

/// A client sends `RepositoryOpen` with its `ClientHello`, without waiting for the
/// session. When the token is refused, that request is still unread: the server
/// must close the connection with a closing handshake, not reset it, or the client
/// can lose the refusal.
#[tokio::test]
async fn a_refused_session_ends_cleanly_with_a_request_behind_it() {
    let server = start(token_file()).await;
    for _ in 0..20 {
        let (mut ws, _) = connect_async(format!("ws://{}", server.addr))
            .await
            .unwrap();
        send_msg(
            &mut ws,
            &WheatProtocolMessage::ClientHello(ClientHelloOutput {
                message_type: 0x02,
                selected_version: 1,
                auth_token: b"not-a-token".to_vec(),
                receive_window_bytes: 16 * 1024 * 1024,
                receive_window_messages: 256,
            }),
        )
        .await;
        send_msg(
            &mut ws,
            &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
                message_type: 0x10,
                name: REPO.to_owned(),
            }),
        )
        .await;

        assert!(matches!(
            recv_msg(&mut ws).await,
            WheatProtocolMessage::ServerHello(_)
        ));
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::SessionEstablished(se) => assert!(
                matches!(
                    se.status,
                    SessionEstablishedStatus::SessionStatusAuthFailed(_)
                ),
                "{se:?}"
            ),
            other => panic!("expected SessionEstablished, got {other:?}"),
        }
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                other => panic!("expected the closing handshake, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn a_repository_without_access_looks_missing() {
    let server = start(token_file()).await;
    let (_ws, hidden) = open(server.addr, b"stranger-token", REPO).await;
    let (_ws, missing) = open(server.addr, b"stranger-token", "team/none").await;
    let (WheatProtocolMessage::Error(hidden), WheatProtocolMessage::Error(missing)) =
        (hidden, missing)
    else {
        panic!("expected two errors");
    };
    assert_eq!(hidden.code, 404);
    assert_eq!(missing.code, 404);
    assert_eq!(
        hidden.message.replace(REPO, "<name>"),
        missing.message.replace("team/none", "<name>"),
        "a hidden repository must be indistinguishable from a missing one"
    );
}

#[tokio::test]
async fn read_access_can_fetch_but_not_publish() {
    let server = start(token_file()).await;
    let (mut ws, state) = open_ok(server.addr, b"reader-token").await;

    send_msg(&mut ws, &fetch_request(1, &state)).await;
    assert!(matches!(
        recv_msg(&mut ws).await,
        WheatProtocolMessage::ObjectBatch(_)
    ));
    loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ObjectBatch(_) => {}
            WheatProtocolMessage::FetchPushEnd(_) => break,
            other => panic!("expected the push, got {other:?}"),
        }
    }
    send_msg(
        &mut ws,
        &WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
            message_type: 0x23,
            op_id: 1,
        }),
    )
    .await;

    let result = publish_reply(&mut ws, &state, false).await;
    assert!(is_permission_denial(&result), "{result:?}");
    assert!(
        result.message.contains("publish access"),
        "{}",
        result.message
    );
}

/// A refused publish still reads the push that follows it, so the session carries
/// on in step.
#[tokio::test]
async fn a_refused_publish_reads_its_push_and_the_session_goes_on() {
    let server = start(token_file()).await;
    let (mut ws, state) = open_ok(server.addr, b"reader-token").await;

    let pushed = b"pushed by a reader".to_vec();
    send_msg(&mut ws, &publish_request(&state, false)).await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
            message_type: 0x44,
            op_id: 7,
            content_id: Digest256 {
                bytes: ContentId::of(&pushed).as_bytes().to_vec(),
            },
            data: pushed,
        }),
    )
    .await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishPushEnd(PublishPushEndOutput {
            message_type: 0x4b,
            op_id: 7,
            pushed_objects: 0,
            pushed_contents: 1,
        }),
    )
    .await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::PublishResult(result) => {
            assert!(is_permission_denial(&result), "{result:?}");
        }
        other => panic!("expected PublishResult, got {other:?}"),
    }

    send_msg(&mut ws, &fetch_request(1, &state)).await;
    assert!(matches!(
        recv_msg(&mut ws).await,
        WheatProtocolMessage::ObjectBatch(_)
    ));
}

#[tokio::test]
async fn publish_access_cannot_force() {
    let server = start(token_file()).await;
    let (mut ws, state) = open_ok(server.addr, b"writer-token").await;

    // Proposing the head itself leaves nothing to upload: past the access check,
    // the object phase ends at once.
    send_publish(&mut ws, &state, false).await;
    let normal = recv_msg(&mut ws).await;
    assert!(
        matches!(normal, WheatProtocolMessage::ObjectsComplete(_)),
        "a writer's publish should reach the object phase: {normal:?}"
    );

    let (mut ws, state) = open_ok(server.addr, b"writer-token").await;
    let forced = publish_reply(&mut ws, &state, true).await;
    assert!(is_permission_denial(&forced), "{forced:?}");
    assert!(
        forced.message.contains("force access"),
        "{}",
        forced.message
    );
}

#[tokio::test]
async fn force_access_can_force() {
    let server = start(token_file()).await;
    let (mut ws, state) = open_ok(server.addr, b"admin-token").await;
    let forced = publish_reply(&mut ws, &state, true).await;
    assert!(
        matches!(
            forced.status,
            PublishResultStatus::PublishStatusStaleHead(_)
        ),
        "an admin's forced publish should reach the head check: {forced:?}"
    );
}

async fn upgrade_with_origin(addr: SocketAddr, origin: Option<&str>) -> Result<(), String> {
    let mut request = format!("ws://{addr}").into_client_request().unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
    }
    connect_async(request)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn browser_origins_must_be_allowed() {
    let server = start(token_file()).await;
    upgrade_with_origin(server.addr, None).await.unwrap();
    upgrade_with_origin(server.addr, Some("https://allowed.example"))
        .await
        .unwrap();
    let error = upgrade_with_origin(server.addr, Some("https://evil.example"))
        .await
        .unwrap_err();
    assert!(error.contains("403"), "{error}");
}

#[tokio::test]
async fn the_open_server_also_refuses_foreign_origins() {
    let server = start(ServerConfig::new()).await;
    let error = upgrade_with_origin(server.addr, Some("https://evil.example"))
        .await
        .unwrap_err();
    assert!(error.contains("403"), "{error}");
}

#[tokio::test]
async fn the_open_server_refuses_non_loopback_addresses() {
    let dir = tempfile::tempdir().unwrap();
    setup_repo(dir.path()).await;
    let registry = Registry::single_named(REPO.to_owned(), dir.path()).unwrap();
    let (_shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, _ready_rx) = tokio::sync::oneshot::channel();
    let error = run_until(
        "0.0.0.0:0".parse().unwrap(),
        registry,
        ServerConfig::new(),
        ready_tx,
        shutdown_rx,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("without authentication"),
        "{error}"
    );
}

/// A custom provider, as an embedder would plug in. Access can be revoked
/// while a session is open, and authentication can fail outright.
struct Revocable {
    granted: AtomicBool,
    broken: bool,
}

#[async_trait]
impl AccessProvider for Revocable {
    async fn authenticate(&self, _token: &[u8]) -> Result<Option<Principal>, AccessError> {
        if self.broken {
            return Err(AccessError("database is down".to_owned()));
        }
        Ok(Some(Principal {
            name: "someone".to_owned(),
        }))
    }

    async fn access(
        &self,
        _principal: &Principal,
        _repository: &str,
    ) -> Result<Option<Access>, AccessError> {
        Ok(self.granted.load(Ordering::SeqCst).then_some(Access::Read))
    }
}

#[tokio::test]
async fn revoked_access_stops_the_next_operation() {
    let provider = Arc::new(Revocable {
        granted: AtomicBool::new(true),
        broken: false,
    });
    let server = start(ServerConfig::with_access(provider.clone(), vec![])).await;
    let (mut ws, state) = open_ok(server.addr, b"anything").await;

    provider.granted.store(false, Ordering::SeqCst);
    send_msg(&mut ws, &fetch_request(3, &state)).await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::Error(e) => {
            assert_eq!((e.op_id, e.code), (3, 403));
            assert!(e.message.contains("permission denied"), "{}", e.message);
        }
        other => panic!("expected Error, got {other:?}"),
    }

    send_msg(
        &mut ws,
        &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
            message_type: 0x30,
            op_id: 4,
            content_ids: vec![Digest256 {
                bytes: ContentId::of(b"hello wheat").as_bytes().to_vec(),
            }],
        }),
    )
    .await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::Error(e) => assert_eq!((e.op_id, e.code), (4, 403)),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failing_provider_refuses_the_session() {
    let provider = Arc::new(Revocable {
        granted: AtomicBool::new(true),
        broken: true,
    });
    let server = start(ServerConfig::with_access(provider, vec![])).await;
    let (_ws, se) = hello(server.addr, b"anything").await;
    assert!(matches!(
        se.status,
        SessionEstablishedStatus::SessionStatusAuthFailed(_)
    ));
    assert_eq!(se.message, "authentication is unavailable");
}
