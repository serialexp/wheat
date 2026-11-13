//! TCP listener and WebSocket upgrade.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{error, info, warn};

use crate::access::{AccessProvider, AuthConfigError, OpenLoopbackAccess, TokenFileAccess};
use crate::config::FileConfig;
use crate::connection;
use crate::hooks::PublishHooks;
use crate::registry::Registry;

/// How long a connection has, by default, from being accepted until its caller is
/// authenticated.
pub const DEFAULT_SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How many connections the server holds open at once by default.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;
/// How long the accept loop pauses after `accept` fails. Running out of file
/// descriptors fails every `accept` at once, and retrying straight away would spin.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

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
    /// How long a connection has from being accepted until its caller is
    /// authenticated: any TLS handshake, the WebSocket upgrade, `ClientHello` and the access
    /// provider's answer together. A client that takes longer, or sends nothing,
    /// is disconnected, so connections nobody has authenticated cannot pile up.
    /// Every entry point in [`connection`] enforces it, so embedders get it too;
    /// for [`connection::serve`] and [`connection::serve_upgraded`] it starts when
    /// they are called, so an embedder bounds its own HTTP upgrade.
    pub session_setup_timeout: Duration,
    /// Most connections [`run`] and [`run_until`] hold open at once. Past it, a
    /// new connection is closed as soon as it is accepted. An embedder that calls
    /// an entry point in [`connection`] itself sets its own limit.
    pub max_connections: usize,
    /// What the embedder runs around each publish: a check before it is joined
    /// onto the head, and a notice once it is durable. `None` runs nothing and
    /// skips the work of describing the publish.
    pub publish_hooks: Option<Arc<dyn PublishHooks>>,
    /// The directory admin operations read bundles and standalone repositories from
    /// and write bundles to ([`crate::admin`]); each is one path segment in it.
    /// `None` refuses them, since an admin could otherwise reach any path the server
    /// can.
    pub exchange_dir: Option<std::path::PathBuf>,
    /// TLS for the connections [`run`] and [`run_until`] accept, which clients reach
    /// with `wheat+tls://` ([`crate::tls::load_pem`]). `None` serves plain
    /// WebSockets, for loopback or behind a proxy that ends TLS. The handshake counts
    /// against [`Self::session_setup_timeout`]. An embedder that does its own upgrade
    /// does its own TLS. Only with the `tls` feature.
    #[cfg(feature = "tls")]
    pub tls: Option<tokio_rustls::TlsAcceptor>,
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
            session_setup_timeout: DEFAULT_SESSION_SETUP_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            publish_hooks: None,
            exchange_dir: None,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// Build the config from the TOML config file. No `[auth]` table means open
    /// and loopback-only; a limit the `[limits]` table leaves out keeps its default.
    pub fn from_file(file: &FileConfig) -> Result<Self, AuthConfigError> {
        let mut config = match &file.auth {
            None => Self::new(),
            Some(auth) => Self::with_access(
                Arc::new(TokenFileAccess::new(auth)?),
                auth.allowed_origins.clone(),
            ),
        };
        if let Some(seconds) = file.limits.session_setup_seconds {
            config.session_setup_timeout = Duration::from_secs(seconds.get());
        }
        if let Some(connections) = file.limits.max_connections {
            config.max_connections = connections.get();
        }
        Ok(config)
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
///
/// Pass the registry in an [`Arc`] to keep a handle on it, for example to
/// [`Registry::quiesce`] a repository.
pub async fn run(
    addr: SocketAddr,
    registry: impl Into<Arc<Registry>>,
    config: ServerConfig,
) -> std::io::Result<()> {
    config.check_listen_address(addr)?;
    let listener = TcpListener::bind(addr).await?;
    let registry = registry.into();
    let config = Arc::new(config);

    info!(addr = %addr, open = config.access.is_open(), "wheat-server listening");
    for name in registry.open_names() {
        info!(repo = name, "serving repository");
    }

    accept_connections(listener, registry, config).await
}

/// Accept connections for ever, serving each on its own task while fewer than
/// [`ServerConfig::max_connections`] are open, and closing it straight away
/// otherwise.
async fn accept_connections(
    listener: TcpListener,
    registry: Arc<Registry>,
    config: Arc<ServerConfig>,
) -> ! {
    let _closing = close_idle_repositories(&registry);
    let slots = Arc::new(Semaphore::new(config.max_connections));
    // Whether the last connection was turned away, so a flood logs once, not per
    // connection.
    let mut full = false;
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => match Arc::clone(&slots).try_acquire_owned() {
                Ok(slot) => {
                    full = false;
                    spawn_connection(stream, slot, &registry, &config);
                }
                Err(_) => {
                    if !full {
                        warn!(
                            peer = %peer,
                            limit = config.max_connections,
                            "closing new connections: too many are open"
                        );
                        full = true;
                    }
                    drop(stream);
                }
            },
            Err(e) => {
                error!(error = %e, "accept failed");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
            }
        }
    }
}

/// Closes the registry's idle repositories periodically, until the returned guard
/// is dropped. A registry without an opener never closes any, and gets no task.
fn close_idle_repositories(registry: &Arc<Registry>) -> Option<AbortOnDrop> {
    let interval = registry.close_interval()?;
    let registry = Arc::clone(registry);
    Some(AbortOnDrop(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            registry.close_idle();
        }
    })))
}

/// Stops a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Serves one accepted connection on its own task, which holds `slot` until the
/// connection ends.
fn spawn_connection(
    stream: TcpStream,
    slot: OwnedSemaphorePermit,
    registry: &Arc<Registry>,
    config: &Arc<ServerConfig>,
) {
    // Nagle's algorithm would hold back the tail of a reply until the client's
    // delayed ACK, which costs every request/reply round tens of milliseconds.
    if let Err(e) = stream.set_nodelay(true) {
        warn!(error = %e, "could not disable Nagle's algorithm");
    }
    let registry = Arc::clone(registry);
    let config = Arc::clone(config);
    tokio::spawn(async move {
        connection::handle_connection(stream, registry, config).await;
        drop(slot);
    });
}

/// Bind the listener, report the bound address, then accept until shutdown.
///
/// The bound address is sent over `ready` immediately after binding. This
/// lets tests discover the port before any connections arrive.
pub async fn run_until(
    addr: SocketAddr,
    registry: impl Into<Arc<Registry>>,
    config: ServerConfig,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    config.check_listen_address(addr)?;
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let registry = registry.into();
    let config = Arc::new(config);

    info!(addr = %bound_addr, "wheat-server listening");

    // Report the bound address so the caller can connect.
    let _ = ready.send(bound_addr);

    tokio::select! {
        _ = accept_connections(listener, registry, config) => {}
        _ = shutdown => {
            info!("shutdown signal received");
        }
    }

    Ok(())
}
