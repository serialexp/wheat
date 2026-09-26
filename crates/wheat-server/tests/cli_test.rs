//! The `wheat-server` binary's own command line: `hash-token` and the refusal
//! to serve an open server on a public address.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn server() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wheat-server"))
}

fn hash_token(stdin: &[u8]) -> Output {
    let mut child = server()
        .arg("hash-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn hash_token_prints_the_hash_of_one_line() {
    let expected = format!("{}\n", blake3::hash(b"secret-token").to_hex());
    for input in [&b"secret-token"[..], b"secret-token\n", b"secret-token\r\n"] {
        let output = hash_token(input);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}

#[test]
fn hash_token_refuses_empty_and_multi_line_input() {
    for input in [&b""[..], b"\n", b"one\ntwo\n"] {
        let output = hash_token(input);
        assert!(!output.status.success(), "accepted {input:?}");
    }
}

#[test]
fn an_open_server_will_not_listen_publicly() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("repo");
    std::fs::create_dir(&data_dir).unwrap();
    let output = server()
        .args(["--listen", "0.0.0.0:0", "--data-dir"])
        .arg(&data_dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("without authentication"), "{stderr}");
}

#[test]
fn a_bad_config_file_stops_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("wheat-server.toml");
    std::fs::write(
        &config,
        "[[auth.principals]]\nname = \"a\"\ntoken_hashes = [\"nope\"]\n",
    )
    .unwrap();
    let output = server()
        .args(["--data-dir"])
        .arg(dir.path())
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("64 hex digits"), "{stderr}");
}
