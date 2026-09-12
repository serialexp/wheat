#![cfg(target_os = "linux")]

use std::{fs, process::Command};

fn command(
    root: &std::path::Path,
    config: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .args(args)
        .output()
        .unwrap()
}

fn ok(root: &std::path::Path, config: &std::path::Path, args: &[&str]) -> std::process::Output {
    let output = command(root, config, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn field(output: &std::process::Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    ok(root.path(), config.path(), &["init"]);
    ok(
        root.path(),
        config.path(),
        &["config", "set", "actor.name", "Test"],
    );
    ok(
        root.path(),
        config.path(),
        &["config", "set", "actor.email", "test@example.invalid"],
    );
    (root, config)
}

#[test]
fn selected_change_survives_reopen_and_drives_flagless_add() {
    let (root, config) = setup();
    let created = ok(
        root.path(),
        config.path(),
        &["change", "new", "-m", "first change"],
    );
    let change = field(&created, "change");
    let listed = ok(root.path(), config.path(), &["changes"]);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains(&format!("* {change} first change")));

    fs::write(root.path().join("file"), "value\n").unwrap();
    let added = ok(root.path(), config.path(), &["add", "file"]);
    let text = String::from_utf8_lossy(&added.stdout);
    assert!(text.contains(&format!("to change {change} — first change")));
    assert!(text.contains("revision "));

    let json = ok(root.path(), config.path(), &["changes", "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let document = &document["result"];
    assert_eq!(document["format"], "wheat-active-changes");
    assert_eq!(document["version"], 1);
    assert_eq!(document["changes"][0]["change"], change);
    assert_eq!(document["selected"], change);
    ok(root.path(), config.path(), &["validate"]);
}

#[test]
fn explicit_add_to_prior_change_keeps_selection_and_absorbs_descendant_edit() {
    let (root, config) = setup();
    let a = ok(root.path(), config.path(), &["change", "new", "-m", "base"]);
    let a_id = field(&a, "change");
    fs::write(root.path().join("file"), "middle\n").unwrap();
    ok(root.path(), config.path(), &["add", "file"]);

    let b = ok(
        root.path(),
        config.path(),
        &["change", "new", "-m", "finish"],
    );
    let b_id = field(&b, "change");
    fs::write(root.path().join("file"), "final\n").unwrap();
    let b_added = ok(root.path(), config.path(), &["add", "file"]);
    let b_revision = field(&b_added, "revision");

    let corrected = ok(
        root.path(),
        config.path(),
        &["add", "--change", &a_id, "file"],
    );
    let text = String::from_utf8_lossy(&corrected.stdout);
    assert!(text.contains(&format!("to change {a_id} — base")));
    assert!(text.contains(&format!("selected change remains {b_id}")));
    assert!(text.contains(&format!("reapplied {b_id}")));

    let listed = ok(root.path(), config.path(), &["changes", "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let document = &document["result"];
    assert_eq!(document["selected"], b_id);
    let b = document["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["change"] == b_id)
        .unwrap();
    assert_eq!(b["revision"], b_revision);
    assert_eq!(b["conflicted"], false);
    ok(root.path(), config.path(), &["validate"]);
}

#[test]
fn change_selection_can_switch_without_changing_repository_authority() {
    let (root, config) = setup();
    let a = ok(
        root.path(),
        config.path(),
        &["change", "new", "-m", "first"],
    );
    let a_id = field(&a, "change");
    let b = ok(
        root.path(),
        config.path(),
        &["change", "new", "-m", "second"],
    );
    let b_id = field(&b, "change");
    let before = fs::read(root.path().join(".wheat/workspace.json")).unwrap();
    let selected = ok(root.path(), config.path(), &["change", "select", &a_id]);
    assert!(String::from_utf8_lossy(&selected.stdout).contains(&a_id));
    let listed = ok(root.path(), config.path(), &["changes", "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let document = &document["result"];
    assert_eq!(document["selected"], a_id);
    assert!(
        document["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["change"] == b_id)
    );
    assert_ne!(
        fs::read(root.path().join(".wheat/workspace.json")).unwrap(),
        before
    );
}

#[test]
fn missing_selection_and_non_tty_message_refuse_without_publishing() {
    let (root, config) = setup();
    let store: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".wheat/workspace.json")).unwrap())
            .unwrap();
    let store_path = std::path::PathBuf::from(store["store"]["path"].as_str().unwrap());
    let head_before = fs::read(store_path.join("repository.head")).unwrap();
    fs::write(root.path().join("file"), "value\n").unwrap();
    let add = command(root.path(), config.path(), &["add", "file"]);
    assert!(!add.status.success());
    assert!(add.stdout.is_empty());
    assert_eq!(
        fs::read(store_path.join("repository.head")).unwrap(),
        head_before
    );

    let create = command(root.path(), config.path(), &["change", "new"]);
    assert!(!create.status.success());
    assert!(String::from_utf8_lossy(&create.stderr).contains("non-interactive"));
    assert_eq!(
        fs::read(store_path.join("repository.head")).unwrap(),
        head_before
    );
}

#[test]
fn imported_history_is_the_basis_and_is_not_listed_active() {
    let source = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(source.path())
        .args(["init", "-q", "-b", "main"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(source.path())
        .args(["config", "user.name", "Test"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(source.path())
        .args(["config", "user.email", "test@example.invalid"])
        .status()
        .unwrap();
    fs::write(source.path().join("file"), "history\n").unwrap();
    Command::new("git")
        .current_dir(source.path())
        .args(["add", "file"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(source.path())
        .args(["commit", "-q", "-m", "history"])
        .status()
        .unwrap();

    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let config = tempfile::tempdir().unwrap();
    ok(
        root.path(),
        config.path(),
        &[
            "import",
            source.path().to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
    );
    let listed = ok(&workspace, config.path(), &["changes", "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let document = &document["result"];
    assert!(document["changes"].as_array().unwrap().is_empty());
}
