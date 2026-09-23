#![cfg(target_os = "linux")]

use std::{fs, path::Path, process::Command};

use serde_json::Value;

fn run(workspace: &Path, stores: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", stores.join("config"))
        .output()
        .unwrap()
}

fn ok(workspace: &Path, stores: &Path, arguments: &[&str]) -> std::process::Output {
    let output = run(workspace, stores, arguments);
    assert!(
        output.status.success(),
        "command failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn store_path(workspace: &Path) -> std::path::PathBuf {
    let metadata: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    metadata["store"]["path"].as_str().unwrap().into()
}

#[test]
fn packed_repack_preserves_shared_workspace_authority_and_materialization() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("git");
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    fs::write(source.join("file.txt"), b"materialized bytes\n").unwrap();
    git(&source, &["add", "file.txt"]);
    git(&source, &["commit", "-m", "root"]);

    ok(
        root.path(),
        &stores,
        &["import", source.to_str().unwrap(), first.to_str().unwrap()],
    );
    ok(
        root.path(),
        &stores,
        &["clone", first.to_str().unwrap(), second.to_str().unwrap()],
    );
    let store = store_path(&first);
    assert_eq!(store, store_path(&second));
    let head_before = fs::read(store.join("repository.head")).unwrap();
    let first_metadata = fs::read(first.join(".wheat/workspace.json")).unwrap();
    let second_metadata = fs::read(second.join(".wheat/workspace.json")).unwrap();
    let first_file = fs::read(first.join("file.txt")).unwrap();
    let second_file = fs::read(second.join("file.txt")).unwrap();

    let repacked = ok(
        root.path(),
        &stores,
        &[
            "--store",
            store.to_str().unwrap(),
            "--format",
            "json",
            "repack",
            "--force",
        ],
    );
    let document: Value = serde_json::from_slice(&repacked.stdout).unwrap();
    assert_eq!(document["command"], "repack");
    assert_eq!(document["result"]["no_op"], false);
    assert_eq!(document["result"]["validated"], true);
    assert_eq!(
        document["result"]["envelope_before"],
        document["result"]["envelope_after"]
    );

    assert_eq!(
        fs::read(store.join("repository.head")).unwrap(),
        head_before
    );
    assert_eq!(
        fs::read(first.join(".wheat/workspace.json")).unwrap(),
        first_metadata
    );
    assert_eq!(
        fs::read(second.join(".wheat/workspace.json")).unwrap(),
        second_metadata
    );
    assert_eq!(fs::read(first.join("file.txt")).unwrap(), first_file);
    assert_eq!(fs::read(second.join("file.txt")).unwrap(), second_file);

    let no_op = ok(&first, &stores, &["repack", "--force"]);
    let human = String::from_utf8(no_op.stdout).unwrap();
    assert!(human.contains("nothing to repack; physical generation"));
    assert!(human.contains("logical objects/content preserved"));
    assert_eq!(
        fs::read(store.join("repository.head")).unwrap(),
        head_before
    );
}

#[test]
fn repack_rejects_loose_store() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let workspace = root.path().join("workspace");
    ok(root.path(), &stores, &["init", workspace.to_str().unwrap()]);

    let output = run(&workspace, &stores, &["repack", "--force"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("repack requires a packed Wheat store")
    );
}
