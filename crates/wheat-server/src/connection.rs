//! Per-connection state machine.
//!
//! Drives the WebSocket through: handshake → repository open → fetch/publish.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{StatusCode, header::ORIGIN};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, info, warn};

use wheat_protocol::*;
use wheat_store::{PublicationStore, RepositoryReader};

use crate::access::{Access, MAX_AUTH_TOKEN_BYTES, Principal};
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

    let allowed_origins = &config.allowed_origins;
    #[allow(clippy::result_large_err)] // The error type is fixed by tungstenite's callback.
    let check_origin =
        |request: &Request, response: Response| check_origin(request, response, allowed_origins);
    // Refuse an oversized message while it arrives, not after it is in memory.
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    let mut ws = match accept_hdr_async_with_config(stream, check_origin, Some(limits)).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!(peer = %peer, error = %e, "WebSocket handshake failed");
            return;
        }
    };

    if let Err(e) = run_session(&mut ws, &registry, &config, &peer).await {
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
    let mut refusal = ErrorResponse::new(Some("origin not allowed".to_owned()));
    *refusal.status_mut() = StatusCode::FORBIDDEN;
    Err(refusal)
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
    op_id: u32,
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

/// The main session loop.
async fn run_session(
    ws: &mut WebSocketStream<TcpStream>,
    registry: &Registry,
    config: &ServerConfig,
    peer: &str,
) -> Result<(), ConnectionError> {
    // --- Session setup ---

    // 1. Send ServerHello.
    send(
        ws,
        &WheatProtocolMessage::ServerHello(ServerHelloOutput {
            message_type: 0x01,
            protocol_version_min: 1,
            protocol_version_max: 1,
            server_id: config.server_id.to_vec(),
            capabilities: vec![],
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
        return Ok(());
    }

    // 3. Authenticate the bearer token.
    let principal = if client_hello.auth_token.len() > MAX_AUTH_TOKEN_BYTES {
        Err("authentication token is too long")
    } else {
        match config.access.authenticate(&client_hello.auth_token).await {
            Ok(Some(principal)) => Ok(principal),
            Ok(None) => Err("authentication failed"),
            Err(e) => {
                warn!(peer = %peer, error = %e, "access provider could not authenticate");
                Err("authentication is unavailable")
            }
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
            return Ok(());
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

    info!(peer = %peer, principal = %principal.name, "session established (v1)");

    // --- Repository open ---

    let (entry, repository) = match recv(ws).await? {
        WheatProtocolMessage::RepositoryOpen(ro) => {
            // A repository the principal may not read looks the same as one that
            // does not exist, so its existence does not leak.
            let readable = repository_access(config, &principal, &ro.name)
                .await?
                .is_some();
            match registry.get(&ro.name).filter(|_| readable) {
                Some(entry) => {
                    // Read the current head and build RepositoryState.
                    let head = entry.heads.read().map_err(|e| {
                        ConnectionError::Internal(format!("failed to read head: {e}"))
                    })?;
                    let reader = RepositoryReader::new(&entry.store, &entry.content);
                    let envelope = reader.envelope(head.envelope).await.map_err(|e| {
                        ConnectionError::Internal(format!("failed to read envelope: {e}"))
                    })?;
                    let labels_manifest = reader
                        .view_label_manifest(envelope.labels)
                        .await
                        .map_err(|e| {
                            ConnectionError::Internal(format!("failed to read labels: {e}"))
                        })?;

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

                    info!(peer = %peer, repo = %ro.name, generation = head.generation, "repository opened");
                    (entry, ro.name)
                }
                None => {
                    send_error(
                        ws,
                        0,
                        NOT_FOUND,
                        operation_error_scope(),
                        false,
                        &format!("repository '{}' not found", ro.name),
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        _ => {
            send_error(
                ws,
                0,
                1,
                connection_error_scope(),
                false,
                "expected RepositoryOpen after session setup",
            )
            .await?;
            return Err(ConnectionError::Protocol("expected RepositoryOpen"));
        }
    };

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
                let needed = if req.force {
                    Access::Force
                } else {
                    Access::Publish
                };
                let granted = repository_access(config, &principal, &repository).await?;
                let allowed = granted.is_some_and(|granted| granted >= needed);
                // The client pushes before it hears anything back, so the push is
                // read even for a publish refused here; it is not kept.
                let pushed = crate::publish::receive_push(ws, req.op_id, allowed).await?;
                // Read head fresh for each publish attempt, with the receipts that
                // answer a publish sent again.
                let (head, receipts) = entry
                    .heads
                    .read_with_receipts()
                    .map_err(|e| ConnectionError::Internal(format!("failed to read head: {e}")))?;
                if !allowed {
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
                crate::publish::handle_publish(ws, &entry, &head, &receipts, req, key, pushed)
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
