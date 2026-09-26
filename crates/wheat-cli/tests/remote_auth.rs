//! `wheat clone` against a wheat-server that needs a token: the token comes
//! from `credential.helper`, and the errors say what to do when it is missing
//! or wrong.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use wheat_server::access::{Access, AuthConfig, PrincipalConfig, token_hash_hex};
use wheat_server::config::FileConfig;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};

const TOKEN: &str = "clone-test-token";

fn wheat(cwd: &Path, stores: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(cwd)
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", stores.join("config"))
        .output()
        .unwrap()
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
    let value: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    PathBuf::from(value["store"]["path"].as_str().unwrap())
}

struct Fixture {
    root: tempfile::TempDir,
    addr: SocketAddr,
    /// Runs the server for as long as the fixture lives.
    _runtime: tokio::runtime::Runtime,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

/// A source repository served by a server that only lets `TOKEN` read it.
fn serve_with_token_file() -> Fixture {
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
            }],
        }),
    })
    .unwrap();
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
        _runtime: runtime,
        _shutdown: shutdown,
    }
}

impl Fixture {
    fn url(&self) -> String {
        format!("wheat://{}/team/repo", self.addr)
    }

    /// Clone into a fresh workspace with its own store home and user config.
    fn clone_with_helper(&self, name: &str, helper: Option<&str>) -> Output {
        let stores = self.root.path().join(format!("{name}-stores"));
        if let Some(helper) = helper {
            ok(
                self.root.path(),
                &stores,
                &["config", "set", "--global", "credential.helper", helper],
            );
        }
        let workspace = self.root.path().join(name);
        wheat(
            self.root.path(),
            &stores,
            &["clone", &self.url(), workspace.to_str().unwrap()],
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
