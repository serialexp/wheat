//! `wheat clone` against a wheat-server that needs a token: the token comes
//! from `credential.helper`, and the errors say what to do when it is missing
//! or wrong. Over `wheat+tls://` the server is checked against the trusted roots,
//! and the token is never sent over plain `wheat://` to another machine.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wheat_server::access::{Access, AuthConfig, PrincipalConfig, token_hash_hex};
use wheat_server::config::FileConfig;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_server::tls::load_pem;

const TOKEN: &str = "clone-test-token";

fn wheat(cwd: &Path, stores: &Path, args: &[&str]) -> Output {
    wheat_trusting(cwd, stores, args, None)
}

/// Runs `wheat`, trusting only the certificates in `roots` (a PEM file) when given.
fn wheat_trusting(cwd: &Path, stores: &Path, args: &[&str], roots: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
    command
        .current_dir(cwd)
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", stores.join("config"));
    if let Some(roots) = roots {
        command
            .env("SSL_CERT_FILE", roots)
            .env_remove("SSL_CERT_DIR");
    }
    command.output().unwrap()
}

/// Writes a self-signed certificate for `localhost` and its key into `dir`, as
/// `<name>.pem` and `<name>.key`, and returns the certificate's path.
fn self_signed(dir: &Path, name: &str) -> PathBuf {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = dir.join(format!("{name}.pem"));
    fs::write(&cert, generated.cert.pem()).unwrap();
    fs::write(
        dir.join(format!("{name}.key")),
        generated.signing_key.serialize_pem(),
    )
    .unwrap();
    cert
}

fn ok(cwd: &Path, stores: &Path, args: &[&str]) -> Output {
    let output = wheat(cwd, stores, args);
    assert!(
        output.status.success(),
        "wheat {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn workspace_store(workspace: &Path) -> PathBuf {
    wheat_client::workspace_metadata::WorkspaceMetadata::read(workspace)
        .unwrap()
        .store
        .path
}

struct Fixture {
    root: tempfile::TempDir,
    addr: SocketAddr,
    /// The server's certificate, when it serves TLS.
    certificate: Option<PathBuf>,
    /// Runs the server for as long as the fixture lives.
    _runtime: tokio::runtime::Runtime,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

/// A source repository served by a server that only lets `TOKEN` read it.
fn serve_with_token_file() -> Fixture {
    serve(false)
}

/// Like [`serve_with_token_file`], over TLS with a self-signed certificate.
fn serve_tls_with_token_file() -> Fixture {
    serve(true)
}

fn serve(tls: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let source_stores = root.path().join("source-stores");
    let source = root.path().join("source");
    ok(
        root.path(),
        &source_stores,
        &["init", source.to_str().unwrap()],
    );
    let store = workspace_store(&source);

    let config = ServerConfig::from_file(&FileConfig {
        auth: Some(AuthConfig {
            allowed_origins: vec![],
            principals: vec![PrincipalConfig {
                name: "cloner".to_owned(),
                token_hashes: vec![token_hash_hex(TOKEN.as_bytes())],
                repositories: [("team/repo".to_owned(), Access::Read)].into(),
                admin: false,
            }],
        }),
        ..FileConfig::default()
    })
    .unwrap();
    let mut config = config;
    let certificate = tls.then(|| {
        let cert = self_signed(root.path(), "server");
        config.tls = Some(load_pem(&cert, &root.path().join("server.key")).unwrap());
        cert
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = Registry::single_named("team/repo".to_owned(), &store).unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
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
    let addr = runtime.block_on(ready_rx).unwrap();
    Fixture {
        root,
        addr,
        certificate,
        _runtime: runtime,
        _shutdown: shutdown,
    }
}

impl Fixture {
    fn url(&self) -> String {
        match self.certificate {
            None => format!("wheat://{}/team/repo", self.addr),
            // The certificate names `localhost`, not the address.
            Some(_) => format!("wheat+tls://localhost:{}/team/repo", self.addr.port()),
        }
    }

    /// Clone into a fresh workspace with its own store home and user config.
    fn clone_with_helper(&self, name: &str, helper: Option<&str>) -> Output {
        self.clone_from(&self.url(), name, helper, self.certificate.as_deref())
    }

    /// Clone `url` into a fresh workspace, trusting only `roots` when given.
    fn clone_from(
        &self,
        url: &str,
        name: &str,
        helper: Option<&str>,
        roots: Option<&Path>,
    ) -> Output {
        let stores = self.root.path().join(format!("{name}-stores"));
        if let Some(helper) = helper {
            ok(
                self.root.path(),
                &stores,
                &["config", "set", "--global", "credential.helper", helper],
            );
        }
        let workspace = self.root.path().join(name);
        wheat_trusting(
            self.root.path(),
            &stores,
            &["clone", url, workspace.to_str().unwrap()],
            roots,
        )
    }
}

#[test]
fn clone_sends_the_token_from_credential_helper() {
    let fixture = serve_with_token_file();
    let helper = format!("printf 'token={TOKEN}\\n' #");
    let output = fixture.clone_with_helper("with-token", Some(&helper));
    assert!(
        output.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.path().join("with-token/.wheat").is_dir());
}

#[test]
fn clone_without_a_helper_says_how_to_add_one() {
    let fixture = serve_with_token_file();
    let output = fixture.clone_with_helper("no-helper", None);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("credential.helper"), "{stderr}");
    assert!(stderr.contains("needs a token"), "{stderr}");
}

#[test]
fn clone_with_a_wrong_token_names_the_helper() {
    let fixture = serve_with_token_file();
    let output = fixture.clone_with_helper("wrong-token", Some("printf 'token=nope\\n' #"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refused the token"), "{stderr}");
}

#[test]
fn a_failing_helper_creates_no_store() {
    let fixture = serve_with_token_file();
    let output = fixture.clone_with_helper("broken-helper", Some("exit 7"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("credential.helper failed"), "{stderr}");
    let stores = fixture.root.path().join("broken-helper-stores");
    let leftovers: Vec<_> = fs::read_dir(&stores)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "config")
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}
#[test]
fn clone_over_tls_checks_the_server_against_the_trusted_roots() {
    let fixture = serve_tls_with_token_file();
    let helper = format!("printf 'token={TOKEN}\\n' #");
    let output = fixture.clone_with_helper("over-tls", Some(&helper));
    assert!(
        output.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root.path().join("over-tls/.wheat").is_dir());
}

#[test]
fn clone_over_tls_refuses_a_server_it_does_not_trust() {
    let fixture = serve_tls_with_token_file();
    let stranger = self_signed(fixture.root.path(), "stranger");
    let helper = format!("printf 'token={TOKEN}\\n' #");
    let output = fixture.clone_from(&fixture.url(), "untrusted", Some(&helper), Some(&stranger));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("certificate"), "{stderr}");
    assert!(!fixture.root.path().join("untrusted/.wheat").exists());
}

#[test]
fn the_token_is_not_sent_over_plain_wheat_to_another_machine() {
    let fixture = serve_with_token_file();
    let helper = format!("printf 'token={TOKEN}\\n' #");
    // A documentation address: nothing answers there, so the refusal has to come
    // before any attempt to connect.
    let output = fixture.clone_from(
        "wheat://192.0.2.1:7420/team/repo",
        "in-the-clear",
        Some(&helper),
        None,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to send the token"), "{stderr}");
    assert!(stderr.contains("wheat+tls://"), "{stderr}");
}
