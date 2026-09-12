#![cfg(target_os = "linux")]

use std::{fs, path::Path, process::Command};

fn command(root: &Path, config: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .env("WHEAT_STORE_HOME", root.join("stores"))
        .args(arguments)
        .output()
        .unwrap()
}

fn field(output: &std::process::Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn document(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert_eq!(value["format"], "wheat-command-result");
    assert_eq!(value["version"], 1);
    value
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let initialized = command(root.path(), config.path(), &["--format", "json", "init"]);
    assert!(initialized.status.success());
    assert_eq!(document(&initialized)["ok"], true);
    for (key, value) in [
        ("actor.name", "Automation Test"),
        ("actor.email", "automation@example.invalid"),
    ] {
        let configured = command(
            root.path(),
            config.path(),
            &["config", "set", key, value, "--format=json"],
        );
        assert!(configured.status.success());
        assert_eq!(document(&configured)["ok"], true);
    }
    (root, config)
}

#[test]
fn global_json_envelopes_read_and_mutation_results() {
    let (root, config) = setup();
    let created = command(
        root.path(),
        config.path(),
        &["change", "--format", "json", "new", "-m", "automation"],
    );
    assert!(created.status.success());
    let created = document(&created);
    assert_eq!(created["ok"], true);
    assert_eq!(created["command"], "change.new");
    assert!(created["result"]["change"].is_string());

    fs::write(root.path().join("file"), "value\n").unwrap();
    let added = command(
        root.path(),
        config.path(),
        &["--format=json", "add", "file"],
    );
    assert!(added.status.success());
    let added = document(&added);
    assert_eq!(added["command"], "add");
    assert_eq!(added["result"]["paths"], 1);
    assert!(added["result"]["revision"].is_string());

    let status = command(root.path(), config.path(), &["status", "--format", "json"]);
    assert!(status.status.success());
    let status = document(&status);
    assert_eq!(status["command"], "status");
    assert_eq!(status["result"]["schema_version"], 1);

    let changes = command(root.path(), config.path(), &["changes", "--format=json"]);
    assert!(changes.status.success());
    let changes = document(&changes);
    assert_eq!(changes["result"]["format"], "wheat-active-changes");
    assert_eq!(changes["result"]["version"], 1);

    let conflicts = command(root.path(), config.path(), &["--format=json", "conflicts"]);
    assert!(conflicts.status.success());
    assert_eq!(document(&conflicts)["result"]["schema_version"], 1);

    let diff = command(root.path(), config.path(), &["diff", "--format=json"]);
    assert!(diff.status.success());
    assert_eq!(document(&diff)["result"]["schema_version"], 1);
}

#[test]
fn parse_and_runtime_failures_are_single_structured_documents() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let parse = command(
        root.path(),
        config.path(),
        &["--format=json", "not-a-command"],
    );
    assert_eq!(parse.status.code(), Some(2));
    let parse = document(&parse);
    assert_eq!(parse["ok"], false);
    assert_eq!(parse["error"]["category"], "usage");
    assert_eq!(parse["error"]["code"], "invalid_arguments");
    assert_eq!(parse["error"]["retryable"], false);

    let missing = command(root.path(), config.path(), &["--format=json", "status"]);
    assert!(!missing.status.success());
    let missing = document(&missing);
    assert_eq!(missing["ok"], false);
    assert!(missing["error"]["code"].is_string());
    assert!(missing["error"]["message"].as_str().unwrap().len() <= 65_560);
}

#[test]
fn stale_predecessor_is_retryable_and_does_not_advance_the_head() {
    let (root, config) = setup();
    let created = command(
        root.path(),
        config.path(),
        &["--format=json", "change", "new", "-m", "stale"],
    );
    assert!(created.status.success());
    let created = document(&created);
    let change = created["result"]["change"].as_str().unwrap();
    let workspace: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".wheat/workspace.json")).unwrap())
            .unwrap();
    let store = Path::new(workspace["store"]["path"].as_str().unwrap());
    let before = fs::read(store.join("repository.head")).unwrap();
    fs::write(root.path().join("file"), "value\n").unwrap();

    let stale = command(
        root.path(),
        config.path(),
        &[
            "--format=json",
            "add",
            "--change",
            change,
            "--predecessor",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "file",
        ],
    );
    assert_eq!(stale.status.code(), Some(3));
    let stale = document(&stale);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["error"]["category"], "state");
    assert_eq!(stale["error"]["code"], "stale_state");
    assert_eq!(stale["error"]["retryable"], true);
    assert_eq!(fs::read(store.join("repository.head")).unwrap(), before);
}

#[test]
fn human_output_and_raw_cat_remain_the_defaults() {
    let (root, config) = setup();
    let status = command(root.path(), config.path(), &["status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("working view:"));
    assert!(!String::from_utf8_lossy(&status.stdout).contains("wheat-command-result"));

    let created = command(
        root.path(),
        config.path(),
        &[
            "change",
            "new",
            "-m",
            "raw content",
            "--path",
            "file",
            "--value",
            "raw bytes",
        ],
    );
    assert!(created.status.success());
    let content_id = wheat_core::ContentId::of(b"raw bytes").to_string();
    let cat = command(root.path(), config.path(), &["cat", &content_id]);
    assert!(cat.status.success());
    assert_eq!(cat.stdout, b"raw bytes");

    let cat_json = command(
        root.path(),
        config.path(),
        &["cat", &content_id, "--format=json"],
    );
    assert!(cat_json.status.success());
    let cat_json = document(&cat_json);
    assert_eq!(cat_json["command"], "cat");
    assert_eq!(cat_json["result"]["content_id"], content_id);
    assert_eq!(
        cat_json["result"]["bytes"],
        serde_json::json!([114, 97, 119, 32, 98, 121, 116, 101, 115])
    );
    assert!(!field(&created, "revision").is_empty());
}

#[test]
fn json_help_is_a_terminal_success_document() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let help = command(root.path(), config.path(), &["--format=json", "--help"]);
    assert!(help.status.success());
    let help = document(&help);
    assert_eq!(help["ok"], true);
    assert_eq!(help["command"], "help");
    assert!(help["result"]["text"].as_str().unwrap().contains("Usage:"));
}
