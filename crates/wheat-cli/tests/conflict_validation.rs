#![cfg(target_os = "linux")]

use std::process::{Command, Output};

mod support;

fn store_home(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace
        .parent()
        .unwrap()
        .join("conflict-validation-stores")
}

fn store_path(workspace: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(store_home(workspace))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}

fn wheat(workspace: &std::path::Path, arguments: &[&str]) -> Output {
    let mut arguments = arguments.to_vec();
    if arguments.starts_with(&["change", "new"]) {
        arguments.extend(["--message", "test revision"]);
    }
    let config_home = workspace.parent().unwrap().join("conflict-test-config");
    std::fs::create_dir_all(config_home.join("wheat")).unwrap();
    std::fs::write(
        config_home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .env("WHEAT_STORE_HOME", store_home(workspace))
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .expect("wheat command runs")
}

fn ok(workspace: &std::path::Path, arguments: &[&str]) -> Output {
    let output = wheat(workspace, arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn corrupt_object(store: &std::path::Path, id: impl Into<wheat_core::ObjectId>) {
    support::corrupt_packed_record(store, *id.into().as_bytes());
}

fn initialized_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ok(&workspace, &["init"]);
    (parent, workspace)
}

// That inspection leaves unselected retained history unread is a unit test beside `inspect`:
// the packed store authenticates its whole active segment on open, so damaging one record on
// disk rejects the store before any command could skip that record.

#[test]
fn conflict_inspection_rejects_a_corrupt_selected_working_view() {
    let (_parent, workspace) = initialized_workspace();
    let status: serde_json::Value =
        serde_json::from_slice(&ok(&workspace, &["status", "--format", "json"]).stdout).unwrap();
    let effective: wheat_core::ViewId =
        serde_json::from_value(status["result"]["effective_view"].clone()).unwrap();
    corrupt_object(&store_path(&workspace), effective);

    let output = wheat(&workspace, &["conflicts"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("immutable object storage integrity failure"),
        "unexpected diagnostic: {stderr}"
    );
}
