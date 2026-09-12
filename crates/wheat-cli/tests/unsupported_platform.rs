#![cfg(not(target_os = "linux"))]

use std::process::Command;

#[test]
fn every_invocation_fails_before_side_effects_on_unsupported_platforms() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root.path())
        .args([
            "--format=json",
            "init",
            workspace.to_str().expect("temporary path is UTF-8"),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["format"], "wheat-command-result");
    assert_eq!(document["version"], 1);
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["category"], "platform");
    assert_eq!(document["error"]["code"], "unsupported_platform");
    assert_eq!(document["error"]["retryable"], false);
    assert!(!workspace.exists());
}
