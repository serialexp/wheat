//! Per-connection state machine.
//!
//! Drives the WebSocket through: handshake → repository open → fetch/publish.

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tracing::{debug, info, warn};

use wheat_protocol::*;
use wheat_store::{PublicationStore, RepositoryReader};

use crate::registry::Registry;
use crate::server::ServerConfig;
use crate::wire::{
    connection_error_scope, operation_error_scope, recv, send, send_error,
};

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

    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!(peer = %peer, error = %e, "WebSocket handshake failed");
            return;
        }
    };

    if let Err(e) = run_session(ws, &registry, &config, &peer).await {
        info!(peer = %peer, error = %e, "session ended with error");
    } else {
        debug!(peer = %peer, "session closed cleanly");
    }
}

/// The main session loop.
async fn run_session(
    mut ws: WebSocketStream<TcpStream>,
    registry: &Registry,
    config: &ServerConfig,
    peer: &str,
) -> Result<(), ConnectionError> {
    // --- Session setup ---

    // 1. Send ServerHello.
    send(
        &mut ws,
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
    let client_hello = match recv(&mut ws).await? {
        WheatProtocolMessage::ClientHello(ch) => ch,
        _other => {
            send_error(
                &mut ws,
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
            &mut ws,
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

    // 3. Send SessionEstablished.
    send(
        &mut ws,
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

    info!(peer = %peer, "session established (v1)");

    // --- Repository open ---

    let entry = match recv(&mut ws).await? {
        WheatProtocolMessage::RepositoryOpen(ro) => {
            match registry.get(&ro.name) {
                Some(entry) => {
                    // Read the current head and build RepositoryState.
                    let head = entry.heads.read().map_err(|e| {
                        ConnectionError::Internal(format!("failed to read head: {e}"))
                    })?;
                    let reader = RepositoryReader::new(&entry.objects, &entry.content);
                    let envelope = reader.envelope(head.envelope).await.map_err(|e| {
                        ConnectionError::Internal(format!("failed to read envelope: {e}"))
                    })?;
                    let labels_manifest =
                        reader.view_label_manifest(envelope.labels).await.map_err(|e| {
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
                        &mut ws,
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
                    entry
                }
                None => {
                    send_error(
                        &mut ws,
                        0,
                        404,
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
                &mut ws,
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
    // when concurrent clients publish.

    loop {
        let msg = match recv(&mut ws).await {
            Ok(msg) => msg,
            Err(ConnectionError::Closed) => break,
            Err(e) => return Err(e),
        };

        match msg {
            WheatProtocolMessage::FetchRequest(req) => {
                // Read head fresh for each fetch.
                let head = entry.heads.read().map_err(|e| {
                    ConnectionError::Internal(format!("failed to read head: {e}"))
                })?;
                crate::fetch::handle_fetch(&mut ws, &entry, &head, req).await?;
            }
            WheatProtocolMessage::ContentFetchRequest(req) => {
                crate::fetch::handle_content_fetch(&mut ws, &entry, req).await?;
            }
            WheatProtocolMessage::PublishRequest(req) => {
                // Read head fresh for each publish attempt.
                let head = entry.heads.read().map_err(|e| {
                    ConnectionError::Internal(format!("failed to read head: {e}"))
                })?;
                crate::publish::handle_publish(&mut ws, &entry, &head, req).await?;
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
