//! Serving `wheat+tls://` directly: the server ends TLS itself, within the
//! session-setup deadline, with a certificate and key loaded from PEM files.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async, connect_async};

use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_server::tls::load_pem;

const REPO: &str = "team/repo";
/// How long a test waits for the server to do something it should do well before.
const PATIENCE: Duration = Duration::from_secs(30);

/// A self-signed certificate for `localhost`, written out as PEM files.
struct Certificate {
    dir: tempfile::TempDir,
    der: CertificateDer<'static>,
}

impl Certificate {
    fn new() -> Self {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cert.pem"), generated.cert.pem()).unwrap();
        std::fs::write(
            dir.path().join("key.pem"),
            generated.signing_key.serialize_pem(),
        )
        .unwrap();
        Self {
            dir,
            der: generated.cert.der().clone(),
        }
    }

    fn cert(&self) -> PathBuf {
        self.dir.path().join("cert.pem")
    }

    fn key(&self) -> PathBuf {
        self.dir.path().join("key.pem")
    }
}

struct Server {
    addr: SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

/// Serves an empty repository over TLS with `certificate`.
async fn start(certificate: &Certificate, session_setup_timeout: Duration) -> Server {
    let mut config = ServerConfig::new();
    config.tls = Some(load_pem(&certificate.cert(), &certificate.key()).unwrap());
    config.session_setup_timeout = session_setup_timeout;
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::single_named(REPO.to_owned(), dir.path()).unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        run_until(
            "127.0.0.1:0".parse().unwrap(),
            registry,
            config,
            ready_tx,
            shutdown_rx,
        )
        .await
        .unwrap();
    });
    Server {
        addr: ready_rx.await.unwrap(),
        _shutdown: shutdown,
        _dir: dir,
    }
}

/// A TLS client that trusts only `certificate`.
fn connector(certificate: &Certificate) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.der.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn a_client_that_trusts_the_certificate_gets_a_session() {
    let certificate = Certificate::new();
    let server = start(&certificate, PATIENCE).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let tls = connector(&certificate)
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let url = format!("wss://localhost:{}/{REPO}", server.addr.port());
    let (mut ws, _) = client_async(url, tls).await.unwrap();
    let first = tokio::time::timeout(PATIENCE, ws.next())
        .await
        .expect("the server said nothing")
        .expect("the server closed the session")
        .unwrap();
    let Message::Binary(bytes) = first else {
        panic!("expected a binary message, got {first:?}");
    };
    assert!(matches!(
        decode_message(&bytes).unwrap(),
        WheatProtocolMessage::ServerHello(_)
    ));
}

#[tokio::test]
async fn a_plain_websocket_gets_no_session_from_a_tls_server() {
    let certificate = Certificate::new();
    let server = start(&certificate, PATIENCE).await;
    let url = format!("ws://{}/{REPO}", server.addr);
    let attempt = tokio::time::timeout(PATIENCE, connect_async(url))
        .await
        .expect("the server neither answered nor closed");
    assert!(attempt.is_err());
}

#[tokio::test]
async fn a_connection_that_never_starts_tls_is_closed_at_the_setup_deadline() {
    let certificate = Certificate::new();
    let server = start(&certificate, Duration::from_millis(200)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
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

fn load_error(cert: &Path, key: &Path) -> String {
    match load_pem(cert, key) {
        Ok(_) => panic!("loaded {} and {}", cert.display(), key.display()),
        Err(error) => error.to_string(),
    }
}

#[test]
fn certificates_and_keys_that_cannot_be_used_are_named() {
    let certificate = Certificate::new();
    let other = Certificate::new();
    let missing = certificate.dir.path().join("missing.pem");

    let error = load_error(&missing, &certificate.key());
    assert!(error.contains("missing.pem"), "{error}");

    // A key file is not a certificate chain.
    let error = load_error(&certificate.key(), &certificate.key());
    assert!(error.contains("holds no PEM certificate"), "{error}");

    let error = load_error(&certificate.cert(), &certificate.cert());
    assert!(error.contains("cannot read a private key"), "{error}");

    let error = load_error(&certificate.cert(), &other.key());
    assert!(error.contains("do not make a usable pair"), "{error}");
}
