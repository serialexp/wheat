//! TCP listener and WebSocket upgrade.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::{error, info};

use crate::connection;
use crate::registry::Registry;

/// Server-wide configuration.
pub struct ServerConfig {
    /// Random 32-byte server identity, sent in ServerHello.
    pub server_id: [u8; 32],
}

impl ServerConfig {
    /// Create a new config with a random server ID.
    pub fn new() -> Self {
        let mut server_id = [0u8; 32];
        // Use blake3 of the current PID + timestamp as a cheap unique ID.
        // This doesn't need to be cryptographically random — it's just an
        // identifier to distinguish server instances.
        let seed = format!(
            "wheat-server-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        server_id.copy_from_slice(blake3::hash(seed.as_bytes()).as_bytes());
        Self { server_id }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the server: bind to addr, accept connections, spawn handlers.
pub async fn run(
    addr: SocketAddr,
    registry: Registry,
    config: ServerConfig,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let registry = Arc::new(registry);
    let config = Arc::new(config);

    info!(addr = %addr, repos = registry.len(), "wheat-server listening");
    for name in registry.names() {
        info!(repo = name, "serving repository");
    }

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let registry = Arc::clone(&registry);
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    connection::handle_connection(stream, registry, config).await;
                });
            }
            Err(e) => {
                error!(error = %e, "accept failed");
            }
        }
    }
}

/// Bind the listener, report the bound address, then accept until shutdown.
///
/// The bound address is sent over `ready` immediately after binding. This
/// lets tests discover the port before any connections arrive.
pub async fn run_until(
    addr: SocketAddr,
    registry: Registry,
    config: ServerConfig,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let registry = Arc::new(registry);
    let config = Arc::new(config);

    info!(addr = %bound_addr, repos = registry.len(), "wheat-server listening");

    // Report the bound address so the caller can connect.
    let _ = ready.send(bound_addr);

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let registry = Arc::clone(&registry);
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            connection::handle_connection(stream, registry, config).await;
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "accept failed");
                    }
                }
            }
        } => {}
        _ = shutdown => {
            info!("shutdown signal received");
        }
    }

    Ok(())
}
