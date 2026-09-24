#![cfg(target_os = "linux")]
//! A reader that closes Wheat's output early (`wheat … | head`) ends the command like
//! SIGPIPE would: exit status 141 and no message. A closed stderr loses diagnostics
//! but never panics.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

/// Exit status of a process killed by SIGPIPE (128 + 13).
const BROKEN_PIPE_EXIT: i32 = 141;

fn wheat(home: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
    command
        .args(arguments)
        .env("WHEAT_STORE_HOME", home.join("stores"))
        .env("XDG_CONFIG_HOME", home.join("config"));
    command
}

/// A pipe whose reader is already closed, so the first write fails with `EPIPE`.
fn closed_pipe() -> Stdio {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    writer.into()
}

fn with_closed_stdout(mut command: Command) -> Output {
    command
        .stdout(closed_pipe())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn with_closed_stderr(mut command: Command) -> Output {
    command
        .stdout(Stdio::piped())
        .stderr(closed_pipe())
        .output()
        .unwrap()
}

#[test]
fn closed_stdout_exits_like_sigpipe_without_a_message() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let initialized = wheat(home.path(), &["init", workspace.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let workspace = workspace.to_str().unwrap();

    for arguments in [
        // Help text, which used to panic with "failed printing to stdout".
        &["--help"][..],
        &["change", "new", "--help"],
        // Human command output.
        &["--workspace", workspace, "status"],
        // A structured success and a structured error.
        &["--format", "json", "--workspace", workspace, "status"],
        &[
            "--format",
            "json",
            "--workspace",
            workspace,
            "cat",
            "not-an-id",
        ],
    ] {
        let output = with_closed_stdout(wheat(home.path(), arguments));
        assert_eq!(
            output.status.code(),
            Some(BROKEN_PIPE_EXIT),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn closed_stderr_keeps_the_exit_status() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing");
    for arguments in [
        &["not-a-command"][..],
        &["--workspace", missing.to_str().unwrap(), "status"],
    ] {
        let expected = wheat(home.path(), arguments).output().unwrap();
        assert!(!expected.status.success(), "{arguments:?}");
        assert!(!expected.stderr.is_empty(), "{arguments:?}");
        let output = with_closed_stderr(wheat(home.path(), arguments));
        assert_eq!(
            output.status.code(),
            expected.status.code(),
            "{arguments:?}"
        );
    }
}
