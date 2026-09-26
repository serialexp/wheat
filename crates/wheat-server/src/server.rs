//! TCP listener and WebSocket upgrade.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::{error, info};

use crate::access::{AccessProvider, AuthConfigError, OpenLoopbackAccess, TokenFileAccess};
use crate::config::FileConfig;
use crate::connection;
use crate::registry::Registry;

/// Server-wide configuration.
pub struct ServerConfig {
    /// Random 32-byte server identity, sent in ServerHello.
    pub server_id: [u8; 32],
    /// Who may connect and what they may do.
    pub access: Arc<dyn AccessProvider>,
    /// Browser `Origin` values allowed to open a session. A WebSocket upgrade
    /// that carries any other `Origin` is refused, so a web page cannot drive
    /// the server through a visitor's browser. Requests without `Origin`
    /// (non-browser clients) are not affected.
    pub allowed_origins: Vec<String>,
}

impl ServerConfig {
    /// Create an open, loopback-only config with a fresh server ID.
    pub fn new() -> Self {
        Self::with_access(Arc::new(OpenLoopbackAccess), Vec::new())
    }

    /// Create a config with the given access provider and origin allowlist.
    pub fn with_access(access: Arc<dyn AccessProvider>, allowed_origins: Vec<String>) -> Self {
        Self {
            server_id: fresh_server_id(),
            access,
            allowed_origins,
        }
    }

    /// Build the config from the TOML config file. No `[auth]` table means open
    /// and loopback-only.
    pub fn from_file(file: &FileConfig) -> Result<Self, AuthConfigError> {
        Ok(match &file.auth {
            None => Self::new(),
            Some(auth) => Self::with_access(
                Arc::new(TokenFileAccess::new(auth)?),
                auth.allowed_origins.clone(),
            ),
        })
    }

    /// Refuse to serve an open provider on a non-loopback address. `run` and
    /// `run_until` check this themselves; call it to fail before other setup.
    pub fn check_listen_address(&self, addr: SocketAddr) -> std::io::Result<()> {
        if self.access.is_open() && !addr.ip().is_loopback() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to listen on {addr} without authentication; \
                     listen on a loopback address or add an [auth] table to the config file"
                ),
            ));
        }
        Ok(())
    }
}

fn fresh_server_id() -> [u8; 32] {
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
    *blake3::hash(seed.as_bytes()).as_bytes()
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
    config.check_listen_address(addr)?;
    let listener = TcpListener::bind(addr).await?;
    let registry = Arc::new(registry);
    let config = Arc::new(config);

    info!(addr = %addr, repos = registry.len(), open = config.access.is_open(), "wheat-server listening");
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
    config.check_listen_address(addr)?;
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
