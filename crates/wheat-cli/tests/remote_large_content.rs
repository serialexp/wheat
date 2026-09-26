#![cfg(target_os = "linux")]

//! A file larger than one protocol message goes through `wheat publish` and
//! `wheat clone`: the client uploads it in chunks, the server puts them back
//! together, and a fresh clone downloads it in chunks again.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};

fn ok(cwd: &Path, stores: &Path, config: &Path, args: &[&str]) -> Output {
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

fn workspace_store(workspace: &Path) -> PathBuf {
    let value: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    PathBuf::from(value["store"]["path"].as_str().unwrap())
}

fn git(cwd: &Path, args: &[&str]) {
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

/// Bytes that do not repeat, so nothing about the transfer depends on them
/// compressing well.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[test]
fn a_file_larger_than_a_message_publishes_and_clones() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    fs::create_dir_all(config.join("wheat")).unwrap();
    fs::write(
        config.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":2,"actor":{"name":"Test","email":"test@example.com"}}"#,
    )
    .unwrap();

    let server_stores = root.path().join("server-stores");
    let source = root.path().join("source");
    ok(
        root.path(),
        &server_stores,
        &config,
        &["init", source.to_str().unwrap()],
    );
    let store = workspace_store(&source);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = Registry::single_named("team/repo".to_owned(), &store).unwrap();
    let (_shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
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
    let url = format!("wheat://{addr}/team/repo");

    // Past the 16 MiB message limit, and not a whole number of 4 MiB chunks.
    // `wheat add` stops at 16 MiB, so the file comes in through a Git import.
    let data = noise(17 * 1024 * 1024 + 5);
    let git_work = root.path().join("git-work");
    fs::create_dir_all(&git_work).unwrap();
    git(&git_work, &["init", "-q", "-b", "main"]);
    fs::write(git_work.join("big.bin"), &data).unwrap();
    git(&git_work, &["add", "big.bin"]);
    git(&git_work, &["commit", "-q", "-m", "big file"]);

    let writer_stores = root.path().join("writer-stores");
    let writer = root.path().join("writer");
    ok(
        root.path(),
        &writer_stores,
        &config,
        &[
            "import",
            git_work.to_str().unwrap(),
            writer.to_str().unwrap(),
        ],
    );
    // The imported history is unrelated to the server's, hence --force.
    ok(
        &writer,
        &writer_stores,
        &config,
        &["publish", "--remote", &url, "--force"],
    );

    let reader_stores = root.path().join("reader-stores");
    let reader = root.path().join("reader");
    ok(
        root.path(),
        &reader_stores,
        &config,
        &["clone", &url, reader.to_str().unwrap()],
    );
    let cloned = fs::read(reader.join("big.bin")).unwrap();
    assert!(cloned == data, "cloned file differs from the published one");
}
