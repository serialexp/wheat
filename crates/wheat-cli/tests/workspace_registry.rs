use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

fn wheat(workspace: &Path, stores: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", stores.join("config"))
        .output()
        .unwrap()
}

fn ok(workspace: &Path, stores: &Path, args: &[&str]) -> std::process::Output {
    let output = wheat(workspace, stores, args);
    assert!(
        output.status.success(),
        "command failed: {}",
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

#[test]
fn local_clone_reuses_store_lists_workspaces_and_unregister_preserves_files() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let first = root.path().join("first");
    let second = root.path().join("second");

    ok(root.path(), &stores, &["init", first.to_str().unwrap()]);
    fs::write(first.join("untracked.txt"), b"preserve me\n").unwrap();
    ok(
        root.path(),
        &stores,
        &["clone", first.to_str().unwrap(), second.to_str().unwrap()],
    );

    assert_eq!(workspace_store(&first), workspace_store(&second));
    let first_meta: Value =
        serde_json::from_slice(&fs::read(first.join(".wheat/workspace.json")).unwrap()).unwrap();
    let second_meta: Value =
        serde_json::from_slice(&fs::read(second.join(".wheat/workspace.json")).unwrap()).unwrap();
    assert_ne!(first_meta["workspace_id"], second_meta["workspace_id"]);

    let listed = ok(&first, &stores, &["--format", "json", "workspace", "list"]);
    let document: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let rows = document["result"]["workspaces"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row["current"] == true).count(), 1);
    assert!(rows.iter().all(|row| row["status"] == "attached"));

    ok(&first, &stores, &["workspace", "unregister"]);
    assert!(!first.join(".wheat").exists());
    assert_eq!(
        fs::read(first.join("untracked.txt")).unwrap(),
        b"preserve me\n"
    );
    ok(&second, &stores, &["status"]);

    let listed = ok(&second, &stores, &["--format", "json", "workspace", "list"]);
    let document: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        document["result"]["workspaces"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn copied_workspace_identity_is_rejected_without_replacing_the_original_pin() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let original = root.path().join("original");
    let copied = root.path().join("copied");
    ok(root.path(), &stores, &["init", original.to_str().unwrap()]);
    fs::create_dir(&copied).unwrap();
    fs::create_dir(copied.join(".wheat")).unwrap();
    fs::copy(
        original.join(".wheat/workspace.json"),
        copied.join(".wheat/workspace.json"),
    )
    .unwrap();

    let failed = wheat(&copied, &stores, &["status"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("already active"));

    let listed = ok(
        &original,
        &stores,
        &["--format", "json", "workspace", "list"],
    );
    let document: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let rows = document["result"]["workspaces"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["path"],
        original.canonicalize().unwrap().to_string_lossy().as_ref()
    );
}

#[test]
fn unregister_refuses_unknown_control_entries_without_removing_pin() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let workspace = root.path().join("workspace");
    ok(root.path(), &stores, &["init", workspace.to_str().unwrap()]);
    fs::write(workspace.join(".wheat/unknown"), b"keep").unwrap();

    let failed = wheat(&workspace, &stores, &["workspace", "unregister"]);
    assert!(!failed.status.success());
    assert!(workspace.join(".wheat/workspace.json").is_file());
    assert!(workspace.join(".wheat/unknown").is_file());

    let listed = ok(
        &workspace,
        &stores,
        &["--format", "json", "workspace", "list"],
    );
    let document: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        document["result"]["workspaces"].as_array().unwrap().len(),
        1
    );
}
