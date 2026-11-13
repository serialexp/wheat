//! Per-connection state machine.
//!
//! Drives a connection through: upgrade (the path names the repository) → handshake
//! → repository state → fetch/publish.
//!
//! There are three ways in. [`handle_connection`] takes an accepted TCP connection
//! and does the WebSocket upgrade itself; [`run`](crate::server::run) uses it. An
//! embedder that serves HTTP itself checks the upgrade request with
//! [`Upgrade::check`], then either answers with [`Upgrade::response`] and hands the
//! upgraded socket to [`serve_upgraded`] (hyper's `upgrade::on`), or upgrades with
//! its own WebSocket type and hands that to [`serve`] as a [`Transport`] (axum's
//! `WebSocketUpgrade`).
//!
//! Everything before the caller is authenticated, from the upgrade to the access
//! provider's answer, shares one deadline ([`ServerConfig::session_setup_timeout`]),
//! so a client that connects and then says nothing, or says it slowly, is
//! disconnected instead of holding the connection open. [`handle_connection`] starts
//! it when the connection is accepted; [`serve`] and [`serve_upgraded`], whose
//! caller did the upgrade, when they are called.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request, Response, create_response,
};
use tokio_tungstenite::tungstenite::http::{StatusCode, header::ORIGIN};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async_with_config};
use tracing::{debug, info, warn};

use wheat_protocol::*;

use crate::admin::{self, RunState, StartError};

use crate::access::{Access, Principal};
use crate::receipt::PublishKey;
use crate::registry::Registry;
use crate::server::ServerConfig;
use crate::transport::{Transport, TransportError};
use crate::wire::{
    connection_error_scope, operation_error_scope, publish_rejected_status, publish_result, recv,
    send, send_error, send_error_retry_after,
};

/// Error code for an operation the principal has no access for.
const FORBIDDEN: u16 = 403;
/// Error code for an unknown repository, and for one the principal may not see.
const NOT_FOUND: u16 = 404;
/// Error code for a session that was not set up by its deadline.
const SETUP_TIMED_OUT: u16 = 408;
/// The repository is quiesced; the client may try again.
const UNAVAILABLE: u16 = 503;
/// How long a refusal sent at the setup deadline may take to go out.
const REFUSAL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

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
    #[cfg(not(feature = "tls"))]
    upgrade_and_serve(stream, &registry, &config, &peer, deadline).await;
    #[cfg(feature = "tls")]
    match &config.tls {
        None => upgrade_and_serve(stream, &registry, &config, &peer, deadline).await,
        Some(tls) => match timeout_at(deadline, tls.accept(stream)).await {
            Ok(Ok(stream)) => upgrade_and_serve(stream, &registry, &config, &peer, deadline).await,
            Ok(Err(e)) => warn!(peer = %peer, error = %e, "TLS handshake failed"),
            Err(_) => {
                info!(peer = %peer, "connection closed: no TLS handshake before the deadline");
            }
        },
    }
}

/// Upgrades `stream` to a WebSocket and serves its session, all of setup before
/// `deadline`.
async fn upgrade_and_serve<S>(
    stream: S,
    registry: &Arc<Registry>,
    config: &ServerConfig,
    peer: &str,
    deadline: Instant,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut upgrade = None;
    #[allow(clippy::result_large_err)] // The error type is fixed by tungstenite's callback.
    let check_upgrade = |request: &Request, response: Response| {
        upgrade = Some(Upgrade::check(request, config).map_err(Refusal::into_response)?);
        Ok(response)
    };
    let accept = accept_hdr_async_with_config(stream, check_upgrade, Some(message_limits()));
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
    serve_until(&mut ws, &upgrade, registry, config, peer, deadline).await;
}

/// Serve one session on `io`, a connection the caller has already upgraded to a
/// WebSocket (answering `upgrade`'s request with [`Upgrade::response`]), such as
/// hyper's `upgrade::on`. `peer` names the client in the logs.
pub async fn serve_upgraded<S>(
    io: S,
    upgrade: Upgrade,
    registry: &Arc<Registry>,
    config: &ServerConfig,
    peer: &str,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let deadline = Instant::now() + config.session_setup_timeout;
    let mut ws = WebSocketStream::from_raw_socket(io, Role::Server, Some(message_limits())).await;
    serve_until(&mut ws, &upgrade, registry, config, peer, deadline).await;
}

/// Serve one session on `transport`, a WebSocket the caller upgraded `upgrade`'s
/// request to with its own WebSocket type. `peer` names the client in the logs.
pub async fn serve(
    mut transport: impl Transport,
    upgrade: Upgrade,
    registry: &Arc<Registry>,
    config: &ServerConfig,
    peer: &str,
) {
    let deadline = Instant::now() + config.session_setup_timeout;
    serve_until(&mut transport, &upgrade, registry, config, peer, deadline).await;
}

async fn serve_until(
    ws: &mut impl Transport,
    upgrade: &Upgrade,
    registry: &Arc<Registry>,
    config: &ServerConfig,
    peer: &str,
    deadline: Instant,
) {
    if let Err(e) = run_session(ws, registry, config, peer, upgrade, deadline).await {
        info!(peer = %peer, error = %e, "session ended with error");
    } else {
        debug!(peer = %peer, "session closed cleanly");
    }
    ws.close().await;
}

/// Refuse an oversized message while it arrives, not after it is in memory.
fn message_limits() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE))
}

/// A WebSocket upgrade request that names a repository and comes from an allowed
/// origin. Only [`Upgrade::check`] makes one, so every session went through it.
pub struct Upgrade {
    target: Target,
    request: Request,
}

/// What a session is for.
enum Target {
    Repository(String),
    /// Maintenance operations ([`crate::admin`]), at [`ADMIN_PATH`].
    Admin,
}

impl Upgrade {
    /// Check an upgrade request before upgrading: its path must name a repository
    /// (`/owner/name`) or be the admin path ([`ADMIN_PATH`]), and a browser's `Origin`
    /// must be on
    /// [`ServerConfig::allowed_origins`]. The request is kept for the access
    /// provider, which authenticates it once the client has said hello. An embedder
    /// with an `http::Request<Body>` passes the head:
    /// `Request::from_parts(parts, ())`.
    pub fn check(request: &Request, config: &ServerConfig) -> Result<Self, Refusal> {
        let target = if request.uri().path() == ADMIN_PATH {
            Target::Admin
        } else {
            Target::Repository(
                repository_name(request.uri().path()).map_err(|error| Refusal {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("invalid repository path: {error}"),
                })?,
            )
        };
        check_origin(request, &config.allowed_origins)?;
        Ok(Self {
            target,
            request: copy_request(request),
        })
    }

    /// The repository the request's path names, or `None` for an admin session.
    pub fn repository(&self) -> Option<&str> {
        match &self.target {
            Target::Repository(name) => Some(name),
            Target::Admin => None,
        }
    }

    /// Whether the session is for maintenance operations, not a repository.
    pub fn is_admin(&self) -> bool {
        matches!(self.target, Target::Admin)
    }

    /// The `101 Switching Protocols` answer to the request, for an embedder that
    /// upgrades the connection itself before calling [`serve_upgraded`]. Refuses a
    /// request that is not a valid WebSocket upgrade.
    pub fn response(&self) -> Result<Response, Refusal> {
        create_response(&self.request).map_err(|error| Refusal {
            status: StatusCode::BAD_REQUEST,
            message: format!("not a WebSocket upgrade: {error}"),
        })
    }
}

/// Why an upgrade request was refused, and the HTTP status to answer it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub message: String,
}

impl Refusal {
    fn into_response(self) -> ErrorResponse {
        let mut refusal = ErrorResponse::new(Some(self.message));
        *refusal.status_mut() = self.status;
        refusal
    }
}

/// Refuse a WebSocket upgrade whose `Origin` is not on the allowlist.
///
/// Browsers always send `Origin` on a WebSocket upgrade and let any page open
/// one, so without this a web page could use a visitor's browser to reach a
/// server on their machine. Non-browser clients send no `Origin` and pass.
fn check_origin(request: &Request, allowed_origins: &[String]) -> Result<(), Refusal> {
    let Some(origin) = request.headers().get(ORIGIN) else {
        return Ok(());
    };
    let allowed = origin
        .to_str()
        .is_ok_and(|origin| allowed_origins.iter().any(|allowed| allowed == origin));
    if allowed {
        return Ok(());
    }
    Err(Refusal {
        status: StatusCode::FORBIDDEN,
        message: "origin not allowed".to_owned(),
    })
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
    ws: &mut impl Transport,
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
    ws: &mut impl Transport,
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
    ws: &mut impl Transport,
    registry: &Arc<Registry>,
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
            let _ = tokio::time::timeout(REFUSAL_WAIT, refusal).await;
            return Err(ConnectionError::Protocol(message));
        }
    };
    let Some(principal) = principal else {
        return Ok(());
    };

    // --- Repository open ---
    // The upgrade request's path named the repository; its state follows the session.

    let repository = match &upgrade.target {
        Target::Repository(name) => name.clone(),
        Target::Admin => return run_admin(ws, registry, config, peer, &principal).await,
    };
    // A repository the principal may not read looks the same as one that does not
    // exist, so its existence does not leak.
    let readable = repository_access(config, &principal, &repository)
        .await?
        .is_some();
    // Nothing is opened for a principal who may not read it.
    let entry = if readable {
        registry.get(&repository).await.map_err(|e| {
            ConnectionError::Internal(format!("failed to open repository '{repository}': {e}"))
        })?
    } else {
        None
    };
    let Some(entry) = entry else {
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
    // The labels are in the envelope, which the client fetches.
    send(
        ws,
        &WheatProtocolMessage::RepositoryState(RepositoryStateOutput {
            message_type: 0x11,
            head_generation: head.generation,
            head_envelope: Digest256 {
                bytes: head.envelope.as_bytes().to_vec(),
            },
        }),
    )
    .await?;
    info!(peer = %peer, repo = %repository, generation = head.generation, "repository opened");

    // --- Main command loop ---
    // Read the head fresh on each operation to avoid stale cache issues
    // when concurrent clients publish. Access is also asked afresh, so a
    // revoked right stops the next operation.

    loop {
        let received = tokio::select! {
            received = recv(ws) => received,
            () = entry.removed() => {
                info!(peer = %peer, repo = %repository, "repository deleted; ending the session");
                send_error(
                    ws,
                    0,
                    NOT_FOUND,
                    connection_error_scope(),
                    false,
                    &format!("repository '{repository}' was deleted"),
                )
                .await?;
                break;
            }
        };
        let msg = match received {
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
                // Held until the publish is answered: a quiesce waits for it.
                let Some(_writing) = entry.network.writers.begin() else {
                    info!(peer = %peer, repo = %repository, "publish refused: repository quiesced");
                    send_error_retry_after(
                        ws,
                        req.op_id,
                        UNAVAILABLE,
                        operation_error_scope(),
                        Some(entry.network.writers.retry_after()),
                        &format!(
                            "'{repository}' is not taking publishes for the moment (it is being backed up or moved); publish again shortly"
                        ),
                    )
                    .await?;
                    continue;
                };
                let key = PublishKey::new(&principal, &req)?;
                // Read head fresh for each publish attempt, with the receipt that
                // answers a publish sent again.
                // A receipt the head store may have trimmed early cannot answer, so the
                // publish is published again: a join of what it holds.
                let (head, receipt) = entry
                    .heads
                    .head_with_receipt(key.key())
                    .await
                    .map_err(|e| ConnectionError::Internal(format!("failed to read head: {e}")))?;
                let receipt = receipt.found().copied();
                let publisher = crate::publish::Publisher {
                    repository: &repository,
                    principal: &principal,
                    hooks: config.publish_hooks.as_ref(),
                };
                crate::publish::handle_publish(
                    ws,
                    &entry,
                    &head,
                    receipt.as_ref(),
                    req,
                    key,
                    pushed,
                    publisher,
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

/// Error code for an operation that cannot start because another is running.
const CONFLICT: u16 = 409;
/// Error code for an operation the server is not set up for.
const NOT_IMPLEMENTED: u16 = 501;
/// Error code for a request that is malformed.
const BAD_REQUEST: u16 = 400;

/// An admin session: starts maintenance operations and reports on them. Admin
/// access is asked again before each, so a revoked right stops the next.
async fn run_admin(
    ws: &mut impl Transport,
    registry: &Arc<Registry>,
    config: &ServerConfig,
    peer: &str,
    principal: &Principal,
) -> Result<(), ConnectionError> {
    async fn is_admin(
        config: &ServerConfig,
        principal: &Principal,
    ) -> Result<bool, ConnectionError> {
        config
            .access
            .is_admin(principal)
            .await
            .map_err(|e| ConnectionError::Internal(format!("admin check failed: {e}")))
    }
    if !is_admin(config, principal).await? {
        info!(peer = %peer, principal = %principal.name, "admin session refused");
        send_error(
            ws,
            0,
            FORBIDDEN,
            connection_error_scope(),
            false,
            "permission denied: this needs admin access",
        )
        .await?;
        return Ok(());
    }
    info!(peer = %peer, principal = %principal.name, "admin session opened");
    loop {
        let msg = match recv(ws).await {
            Ok(msg) => msg,
            Err(ConnectionError::Closed) => return Ok(()),
            Err(e) => return Err(e),
        };
        match msg {
            WheatProtocolMessage::AdminStart(request) => {
                if !is_admin(config, principal).await? {
                    send_error(
                        ws,
                        request.op_id,
                        FORBIDDEN,
                        operation_error_scope(),
                        false,
                        "permission denied: this needs admin access",
                    )
                    .await?;
                    continue;
                }
                match admin::start(registry, config.exchange_dir.as_deref(), request.command) {
                    Ok(operation) => {
                        info!(peer = %peer, principal = %principal.name, operation, "operation started");
                        send(
                            ws,
                            &WheatProtocolMessage::AdminStarted(AdminStartedOutput {
                                message_type: 0x51,
                                op_id: request.op_id,
                                operation,
                            }),
                        )
                        .await?;
                    }
                    Err(error) => {
                        let (code, retryable) = match error {
                            StartError::Unsupported(_) => (NOT_IMPLEMENTED, false),
                            StartError::Invalid(_) => (BAD_REQUEST, false),
                            StartError::Busy(_) => (CONFLICT, true),
                        };
                        send_error(
                            ws,
                            request.op_id,
                            code,
                            operation_error_scope(),
                            retryable,
                            &error.to_string(),
                        )
                        .await?;
                    }
                }
            }
            WheatProtocolMessage::AdminStatusRequest(request) => {
                if !is_admin(config, principal).await? {
                    send_error(
                        ws,
                        request.op_id,
                        FORBIDDEN,
                        operation_error_scope(),
                        false,
                        "permission denied: this needs admin access",
                    )
                    .await?;
                    continue;
                }
                let Some(status) = registry.operations().status(request.operation) else {
                    send_error(
                        ws,
                        request.op_id,
                        NOT_FOUND,
                        operation_error_scope(),
                        false,
                        &format!("there is no operation {}", request.operation),
                    )
                    .await?;
                    continue;
                };
                let state = match status.state {
                    RunState::Running => AdminState::AdminRunning(AdminRunningOutput { code: 0 }),
                    RunState::Succeeded => {
                        AdminState::AdminSucceeded(AdminSucceededOutput { code: 1 })
                    }
                    RunState::Failed => AdminState::AdminFailed(AdminFailedOutput { code: 2 }),
                };
                send(
                    ws,
                    &WheatProtocolMessage::AdminStatus(AdminStatusOutput {
                        message_type: 0x53,
                        op_id: request.op_id,
                        operation: request.operation,
                        state,
                        phase: status.phase,
                        message: status.message,
                    }),
                )
                .await?;
            }
            _ => {
                warn!(peer = %peer, "unexpected message in admin session");
            }
        }
    }
}

/// Errors that can occur during a connection.
#[derive(Debug)]
pub enum ConnectionError {
    Transport(TransportError),
    Encode(wheat_protocol::EncodeError),
    Decode(wheat_protocol::DecodeError),
    Protocol(&'static str),
    Internal(String),
    Closed,
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::Transport(e) => write!(f, "transport: {e}"),
            ConnectionError::Encode(e) => write!(f, "encode: {e}"),
            ConnectionError::Decode(e) => write!(f, "decode: {e}"),
            ConnectionError::Protocol(msg) => write!(f, "protocol: {msg}"),
            ConnectionError::Internal(msg) => write!(f, "internal: {msg}"),
            ConnectionError::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for ConnectionError {}
impl From<tokio::task::JoinError> for ConnectionError {
    /// Store work handed to a thread for blocking work panicked or was cancelled.
    fn from(error: tokio::task::JoinError) -> Self {
        ConnectionError::Internal(format!("store work failed: {error}"))
    }
}
