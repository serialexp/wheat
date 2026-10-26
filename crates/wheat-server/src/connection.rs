//! Per-connection state machine.
//!
//! Drives the WebSocket through: upgrade (the path names the repository) → handshake
//! → repository state → fetch/publish.
//!
//! Everything before the caller is authenticated, from the upgrade to the access
//! provider's answer, shares one deadline ([`ServerConfig::session_setup_timeout`]),
//! so a client that connects and then says nothing, or says it slowly, is
//! disconnected instead of holding the connection open.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{StatusCode, header::ORIGIN};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, info, warn};

use wheat_protocol::*;
use wheat_store::RepositoryReader;

use crate::access::{Access, Principal};
use crate::receipt::PublishKey;
use crate::registry::Registry;
use crate::server::ServerConfig;
use crate::wire::{
    connection_error_scope, operation_error_scope, publish_rejected_status, publish_result, recv,
    send, send_error,
};

/// Error code for an operation the principal has no access for.
const FORBIDDEN: u16 = 403;
/// Error code for an unknown repository, and for one the principal may not see.
const NOT_FOUND: u16 = 404;
/// Error code for a session that was not set up by its deadline.
const SETUP_TIMED_OUT: u16 = 408;

/// Run one client connection to completion.
pub async fn handle_connection(
    stream: TcpStream,
    registry: Arc<Registry>,
    config: Arc<ServerConfig>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    debug!(peer = %peer, "new connection");
    let deadline = Instant::now() + config.session_setup_timeout;

    let allowed_origins = &config.allowed_origins;
    let mut upgrade = None;
    #[allow(clippy::result_large_err)] // The error type is fixed by tungstenite's callback.
    let check_upgrade = |request: &Request, response: Response| {
        let repository = repository_name(request.uri().path()).map_err(|error| {
            refusal(
                StatusCode::BAD_REQUEST,
                format!("invalid repository path: {error}"),
            )
        })?;
        let response = check_origin(request, response, allowed_origins)?;
        upgrade = Some(Upgrade {
            repository,
            request: copy_request(request),
        });
        Ok(response)
    };
    // Refuse an oversized message while it arrives, not after it is in memory.
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    let accept = accept_hdr_async_with_config(stream, check_upgrade, Some(limits));
    let mut ws = match timeout_at(deadline, accept).await {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            warn!(peer = %peer, error = %e, "WebSocket handshake failed");
            return;
        }
        Err(_) => {
            info!(peer = %peer, "connection closed: no WebSocket upgrade before the deadline");
            return;
        }
    };
    let Some(upgrade) = upgrade else {
        warn!(peer = %peer, "WebSocket handshake finished without its request");
        return;
    };

    if let Err(e) = run_session(&mut ws, &registry, &config, &peer, &upgrade, deadline).await {
        info!(peer = %peer, error = %e, "session ended with error");
    } else {
        debug!(peer = %peer, "session closed cleanly");
    }
    finish(&mut ws).await;
}

/// How long [`finish`] waits for the client to answer the closing handshake.
const CLOSE_WAIT: Duration = Duration::from_secs(10);

/// Ends the connection with a closing handshake, reading and dropping what the
/// client sent after the server stopped listening: a request sent without waiting
/// for the session, or the rest of a push. Closing the socket with those bytes
/// unread resets the connection, and the client can lose the reply that says why
/// it ended.
async fn finish(ws: &mut WebSocketStream<TcpStream>) {
    // The client closed first: tungstenite has answered it already.
    if ws.close(None).await.is_err() {
        return;
    }
    let drain = async { while let Some(Ok(_)) = ws.next().await {} };
    if tokio::time::timeout(CLOSE_WAIT, drain).await.is_err() {
        debug!("client did not finish the closing handshake");
    }
}

/// Refuse a WebSocket upgrade whose `Origin` is not on the allowlist.
///
/// Browsers always send `Origin` on a WebSocket upgrade and let any page open
/// one, so without this a web page could use a visitor's browser to reach a
/// server on their machine. Non-browser clients send no `Origin` and pass.
#[allow(clippy::result_large_err)] // The error type is fixed by tungstenite's callback.
fn check_origin(
    request: &Request,
    response: Response,
    allowed_origins: &[String],
) -> Result<Response, ErrorResponse> {
    let Some(origin) = request.headers().get(ORIGIN) else {
        return Ok(response);
    };
    let allowed = origin
        .to_str()
        .is_ok_and(|origin| allowed_origins.iter().any(|allowed| allowed == origin));
    if allowed {
        return Ok(response);
    }
    Err(refusal(
        StatusCode::FORBIDDEN,
        "origin not allowed".to_owned(),
    ))
}

/// What a WebSocket upgrade request asked for: the repository its path names, and
/// the request itself, for the access provider.
struct Upgrade {
    repository: String,
    request: Request,
}

/// `request` without its body, which an upgrade request does not have.
fn copy_request(request: &Request) -> Request {
    let mut copy = Request::new(());
    *copy.method_mut() = request.method().clone();
    *copy.uri_mut() = request.uri().clone();
    *copy.version_mut() = request.version();
    *copy.headers_mut() = request.headers().clone();
    copy
}

/// An answer refusing the upgrade with `status`.
fn refusal(status: StatusCode, message: String) -> ErrorResponse {
    let mut refusal = ErrorResponse::new(Some(message));
    *refusal.status_mut() = status;
    refusal
}

/// Ask the provider for the principal's access to `repository`.
async fn repository_access(
    config: &ServerConfig,
    principal: &Principal,
    repository: &str,
) -> Result<Option<Access>, ConnectionError> {
    config
        .access
        .access(principal, repository)
        .await
        .map_err(|e| ConnectionError::Internal(format!("access check failed: {e}")))
}

/// Tell the client that it lacks `needed` access for this operation.
async fn send_forbidden(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u64,
    needed: Access,
    repository: &str,
) -> Result<(), ConnectionError> {
    send_error(
        ws,
        op_id,
        FORBIDDEN,
        operation_error_scope(),
        false,
        &format!("permission denied: this needs {needed} access to '{repository}'"),
    )
    .await
}

/// Set up the session: exchange hellos and authenticate the upgrade request.
/// Returns the principal, or `None` when the session was refused and the client
/// told why.
async fn establish(
    ws: &mut WebSocketStream<TcpStream>,
    config: &ServerConfig,
    peer: &str,
    upgrade: &Upgrade,
) -> Result<Option<Principal>, ConnectionError> {
    // 1. Send ServerHello.
    send(
        ws,
        &WheatProtocolMessage::ServerHello(ServerHelloOutput {
            message_type: 0x01,
            protocol_version_min: 1,
            protocol_version_max: 1,
            server_id: config.server_id.to_vec(),
            capabilities: SUPPORTED_CAPABILITIES
                .iter()
                .map(|&name| name.to_owned())
                .collect(),
        }),
    )
    .await?;

    // 2. Receive ClientHello.
    let client_hello = match recv(ws).await? {
        WheatProtocolMessage::ClientHello(ch) => ch,
        _other => {
            send_error(
                ws,
                0,
                1,
                connection_error_scope(),
                false,
                "expected ClientHello",
            )
            .await?;
            return Err(ConnectionError::Protocol("expected ClientHello"));
        }
    };

    if client_hello.selected_version != 1 {
        send(
            ws,
            &WheatProtocolMessage::SessionEstablished(SessionEstablishedOutput {
                message_type: 0x03,
                selected_version: client_hello.selected_version,
                receive_window_bytes: 0,
                receive_window_messages: 0,
                status: SessionEstablishedStatus::SessionStatusVersionMismatch(
                    SessionStatusVersionMismatchOutput { code: 1 },
                ),
                supported_range_min: 1,
                supported_range_max: 1,
                message: "unsupported protocol version".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }

    // The capabilities the session uses: only ones the server offered.
    let capabilities = match accept_capabilities(&client_hello.capabilities, SUPPORTED_CAPABILITIES)
    {
        Ok(capabilities) => capabilities,
        Err(error) => {
            let message = format!("ClientHello capabilities refused: {error}");
            send_error(ws, 0, 1, connection_error_scope(), false, &message).await?;
            return Err(ConnectionError::Protocol(
                "ClientHello capabilities refused",
            ));
        }
    };

    // 3. Authenticate the upgrade request.
    let principal = match config.access.authenticate(&upgrade.request).await {
        Ok(Some(principal)) => Ok(principal),
        Ok(None) => Err("authentication failed"),
        Err(e) => {
            warn!(peer = %peer, error = %e, "access provider could not authenticate");
            Err("authentication is unavailable")
        }
    };
    let principal = match principal {
        Ok(principal) => principal,
        Err(message) => {
            info!(peer = %peer, "session refused: {message}");
            send(
                ws,
                &WheatProtocolMessage::SessionEstablished(SessionEstablishedOutput {
                    message_type: 0x03,
                    selected_version: 1,
                    receive_window_bytes: 0,
                    receive_window_messages: 0,
                    status: SessionEstablishedStatus::SessionStatusAuthFailed(
                        SessionStatusAuthFailedOutput { code: 2 },
                    ),
                    supported_range_min: 1,
                    supported_range_max: 1,
                    message: message.to_owned(),
                }),
            )
            .await?;
            return Ok(None);
        }
    };

    // 4. Send SessionEstablished.
    send(
        ws,
        &WheatProtocolMessage::SessionEstablished(SessionEstablishedOutput {
            message_type: 0x03,
            selected_version: 1,
            receive_window_bytes: 16 * 1024 * 1024,
            receive_window_messages: 256,
            status: SessionEstablishedStatus::SessionStatusOk(SessionStatusOkOutput { code: 0 }),
            supported_range_min: 1,
            supported_range_max: 1,
            message: String::new(),
        }),
    )
    .await?;

    info!(
        peer = %peer,
        principal = %principal.name,
        capabilities = ?capabilities.to_vec(),
        "session established (v1)"
    );
    Ok(Some(principal))
}

/// The main session loop.
async fn run_session(
    ws: &mut WebSocketStream<TcpStream>,
    registry: &Registry,
    config: &ServerConfig,
    peer: &str,
    upgrade: &Upgrade,
    deadline: Instant,
) -> Result<(), ConnectionError> {
    // --- Session setup ---
    // The deadline that bounded the upgrade also bounds the rest of the setup.

    let principal = match timeout_at(deadline, establish(ws, config, peer, upgrade)).await {
        Ok(principal) => principal?,
        Err(_) => {
            // Tell the client why, but do not wait on one that does not read.
            let message = "session setup took too long";
            let refusal = send_error(
                ws,
                0,
                SETUP_TIMED_OUT,
                connection_error_scope(),
                false,
                message,
            );
            let _ = tokio::time::timeout(CLOSE_WAIT, refusal).await;
            return Err(ConnectionError::Protocol(message));
        }
    };
    let Some(principal) = principal else {
        return Ok(());
    };

    // --- Repository open ---
    // The upgrade request's path named the repository; its state follows the session.

    let repository = upgrade.repository.clone();
    // A repository the principal may not read looks the same as one that does not
    // exist, so its existence does not leak.
    let readable = repository_access(config, &principal, &repository)
        .await?
        .is_some();
    let Some(entry) = registry.get(&repository).filter(|_| readable) else {
        send_error(
            ws,
            0,
            NOT_FOUND,
            operation_error_scope(),
            false,
            &format!("repository '{repository}' not found"),
        )
        .await?;
        return Ok(());
    };
    let head = entry
        .heads
        .head()
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read head: {e}")))?;
    let reader = RepositoryReader::new(&entry.store, &entry.content);
    let envelope = reader
        .envelope(head.envelope)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read envelope: {e}")))?;
    let labels_manifest = reader
        .view_label_manifest(envelope.labels)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read labels: {e}")))?;
    let labels: Vec<LabelEntry> = labels_manifest
        .labels
        .iter()
        .take(10_000)
        .map(|(name, label)| LabelEntry {
            name: name.clone(),
            view_id: Digest256 {
                bytes: label.view.as_bytes().to_vec(),
            },
        })
        .collect();
    let truncated = labels_manifest.labels.len() > 10_000;
    send(
        ws,
        &WheatProtocolMessage::RepositoryState(RepositoryStateOutput {
            message_type: 0x11,
            head_generation: head.generation,
            head_envelope: Digest256 {
                bytes: head.envelope.as_bytes().to_vec(),
            },
            labels,
            labels_truncated: truncated,
        }),
    )
    .await?;
    info!(peer = %peer, repo = %repository, generation = head.generation, "repository opened");

    // --- Main command loop ---
    // Read the head fresh on each operation to avoid stale cache issues
    // when concurrent clients publish. Access is also asked afresh, so a
    // revoked right stops the next operation.

    loop {
        let msg = match recv(ws).await {
            Ok(msg) => msg,
            Err(ConnectionError::Closed) => break,
            Err(e) => return Err(e),
        };

        match msg {
            WheatProtocolMessage::FetchRequest(req) => {
                if repository_access(config, &principal, &repository)
                    .await?
                    .is_none()
                {
                    send_forbidden(ws, req.op_id, Access::Read, &repository).await?;
                    continue;
                }
                // The client names the envelope it wants (the head it saw), so a
                // publish in between does not change what it gets.
                crate::fetch::handle_fetch(ws, &entry, req).await?;
            }
            WheatProtocolMessage::ContentFetchRequest(req) => {
                if repository_access(config, &principal, &repository)
                    .await?
                    .is_none()
                {
                    send_forbidden(ws, req.op_id, Access::Read, &repository).await?;
                    continue;
                }
                crate::fetch::handle_content_fetch(ws, &entry, req).await?;
            }
            WheatProtocolMessage::PublishRequest(req) => {
                let needed = if req.force || req.force_labels {
                    Access::Force
                } else {
                    Access::Publish
                };
                let granted = repository_access(config, &principal, &repository).await?;
                let allowed = granted.is_some_and(|granted| granted >= needed);
                // The client pushes before it hears anything back, so the push is
                // read even for a publish refused here; it is not kept.
                let pushed = crate::publish::receive_push(ws, req.op_id, allowed).await?;
                if !allowed {
                    let head = entry.heads.head().await.map_err(|e| {
                        ConnectionError::Internal(format!("failed to read head: {e}"))
                    })?;
                    // Refuse before anything uploaded is kept.
                    info!(peer = %peer, principal = %principal.name, repo = %repository, %needed, "publish refused");
                    send(
                        ws,
                        &publish_result(
                            req.op_id,
                            publish_rejected_status(),
                            head.generation,
                            head.envelope.as_bytes(),
                            &format!(
                                "permission denied: this publish needs {needed} access to '{repository}'"
                            ),
                        ),
                    )
                    .await?;
                    continue;
                }
                let key = PublishKey::new(&principal, &req)?;
                // Read head fresh for each publish attempt, with the receipt that
                // answers a publish sent again.
                let (head, receipt) = entry
                    .heads
                    .head_with_receipt(key.key())
                    .await
                    .map_err(|e| ConnectionError::Internal(format!("failed to read head: {e}")))?;
                crate::publish::handle_publish(
                    ws,
                    &entry,
                    &head,
                    receipt.as_ref(),
                    req,
                    key,
                    pushed,
                )
                .await?;
            }
            _ => {
                warn!(peer = %peer, "unexpected message in command loop");
            }
        }
    }

    Ok(())
}

/// Errors that can occur during a connection.
#[derive(Debug)]
pub enum ConnectionError {
    WebSocket(tokio_tungstenite::tungstenite::Error),
    Encode(wheat_protocol::EncodeError),
    Decode(wheat_protocol::DecodeError),
    Protocol(&'static str),
    Internal(String),
    Closed,
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::WebSocket(e) => write!(f, "websocket: {e}"),
            ConnectionError::Encode(e) => write!(f, "encode: {e}"),
            ConnectionError::Decode(e) => write!(f, "decode: {e}"),
            ConnectionError::Protocol(msg) => write!(f, "protocol: {msg}"),
            ConnectionError::Internal(msg) => write!(f, "internal: {msg}"),
            ConnectionError::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for ConnectionError {}

impl From<tokio_tungstenite::tungstenite::Error> for ConnectionError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        ConnectionError::WebSocket(e)
    }
}
