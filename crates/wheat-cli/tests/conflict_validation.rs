use std::process::{Command, Output};

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

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn object_path(store: &std::path::Path, encoded: &str) -> std::path::PathBuf {
    store
        .join("objects")
        .join(&encoded[..2])
        .join(&encoded[2..])
}

fn initialized_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ok(&workspace, &["init"]);
    (parent, workspace)
}

#[test]
fn conflict_inspection_does_not_read_unselected_retained_history() {
    let (_parent, workspace) = initialized_workspace();
    let revision = field(
        &ok(
            &workspace,
            &["change", "new", "--path", "file", "--value", "value"],
        ),
        "revision",
    );

    // This revision remains retained, but an empty conflict inspection has no reason to read it.
    // A complete repository validation would encounter and reject these bytes.
    std::fs::write(object_path(&store_path(&workspace), &revision), b"corrupt").unwrap();
    let output = ok(&workspace, &["conflicts", "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["conflicts"], serde_json::json!([]));
}

#[test]
fn conflict_inspection_rejects_a_corrupt_selected_working_view() {
    let (_parent, workspace) = initialized_workspace();
    let status: serde_json::Value =
        serde_json::from_slice(&ok(&workspace, &["status", "--format", "json"]).stdout).unwrap();
    let working: wheat_core::ViewId =
        serde_json::from_value(status["working_view"].clone()).unwrap();
    std::fs::write(
        object_path(&store_path(&workspace), &working.to_string()),
        b"corrupt",
    )
    .unwrap();

    let output = wheat(&workspace, &["conflicts"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("integrity verification"));
}
