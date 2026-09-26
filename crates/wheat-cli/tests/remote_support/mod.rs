//! Helpers for tests that run `wheat` against an in-process wheat-server.
//!
//! Each test binary uses a different part of this module.
#![allow(dead_code)]

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};

/// Runs `wheat` in `cwd` and insists that it succeeds.
pub fn ok(cwd: &Path, stores: &Path, config: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(cwd)
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wheat {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs `wheat` in `cwd`, insists that it fails, and returns what it printed.
pub fn fails(cwd: &Path, stores: &Path, config: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(cwd)
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", config)
        .output()
        .unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "wheat {args:?} succeeded:\n{printed}"
    );
    printed
}

/// Runs `wheat --format json` and returns its `result`.
pub fn ok_json(cwd: &Path, stores: &Path, config: &Path, args: &[&str]) -> Value {
    let args: Vec<&str> = ["--format", "json"].iter().chain(args).copied().collect();
    let output = ok(cwd, stores, config, &args);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    document["result"].clone()
}

/// Writes a Wheat config with a test actor under `config`.
pub fn write_config(config: &Path) {
    fs::create_dir_all(config.join("wheat")).unwrap();
    fs::write(
        config.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":2,"actor":{"name":"Test","email":"test@example.com"}}"#,
    )
    .unwrap();
}

pub fn workspace_store(workspace: &Path) -> PathBuf {
    let value: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    PathBuf::from(value["store"]["path"].as_str().unwrap())
}

pub fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A wheat-server serving one empty repository as `team/repo`. It stops when
/// this is dropped.
pub struct Server {
    pub url: String,
    _runtime: tokio::runtime::Runtime,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

/// Creates an empty repository under `root` and serves it.
pub fn serve(root: &Path, config: &Path) -> Server {
    let server_stores = root.join("server-stores");
    let source = root.join("source");
    ok(
        root,
        &server_stores,
        config,
        &["init", source.to_str().unwrap()],
    );
    let store = workspace_store(&source);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = Registry::single_named("team/repo".to_owned(), &store).unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        run_until(
            "127.0.0.1:0".parse().unwrap(),
            registry,
            ServerConfig::new(),
            ready_tx,
            shutdown_rx,
        )
        .await
        .unwrap();
    });
    let addr: SocketAddr = runtime.block_on(ready_rx).unwrap();
    Server {
        url: format!("wheat://{addr}/team/repo"),
        _runtime: runtime,
        _shutdown: shutdown,
    }
}
