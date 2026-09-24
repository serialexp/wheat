#![cfg(target_os = "linux")]

//! `wheat diff PATH...` limits every comparison mode to the selected paths.

use std::process::{Command, Output};

fn wheat(root: &std::path::Path, arguments: &[&str]) -> Output {
    let mut arguments = arguments.to_vec();
    if arguments.starts_with(&["change", "new"]) || arguments.first() == Some(&"add") {
        arguments.extend(["--message", "test revision"]);
    }
    let config_home = root.parent().unwrap().join("test-config");
    std::fs::create_dir_all(config_home.join("wheat")).unwrap();
    std::fs::write(
        config_home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(root)
        .args(arguments)
        .env(
            "WHEAT_STORE_HOME",
            root.parent().unwrap().join("test-stores"),
        )
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .expect("wheat command runs")
}

fn ok(root: &std::path::Path, arguments: &[&str]) -> Output {
    let output = wheat(root, arguments);
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
        .expect("output field exists")
        .to_owned()
}

/// Paths the JSON document of `wheat diff ARGS --format json` reports.
fn diffed_paths(root: &std::path::Path, arguments: &[&str]) -> Vec<String> {
    let mut arguments = arguments.to_vec();
    arguments.extend(["--format", "json"]);
    let output = ok(root, &arguments);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    document["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_owned())
        .collect()
}

const FILES: [&str; 4] = ["src/a", "src/sub/b", "docs/c", "src-x"];

/// A repository whose latest revision creates `FILES`, with every one of them edited again in
/// the materialized workspace.
fn setup() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    ok(&root, &["init"]);
    ok(
        &root,
        &["change", "new", "--path", "seed", "--value", "seed\n"],
    );
    ok(&root, &["materialize"]);
    for path in FILES {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, format!("{path} one\n")).unwrap();
    }
    let mut add = vec!["add"];
    add.extend(FILES);
    let revision = field(&ok(&root, &add), "revision");
    for path in FILES {
        std::fs::write(root.join(path), format!("{path} two\n")).unwrap();
    }
    (parent, root, revision)
}

#[test]
fn workspace_diff_reports_only_selected_files_and_directories() {
    let (_parent, root, _) = setup();
    assert_eq!(diffed_paths(&root, &["diff"]).len(), 4);
    assert_eq!(
        diffed_paths(&root, &["diff", "src"]),
        ["src/a", "src/sub/b"]
    );
    assert_eq!(
        diffed_paths(&root, &["diff", "src/"]),
        ["src/a", "src/sub/b"]
    );
    assert_eq!(
        diffed_paths(&root, &["diff", "src/sub/b", "docs"]),
        ["docs/c", "src/sub/b"]
    );
    assert!(diffed_paths(&root, &["diff", "missing"]).is_empty());

    let human = String::from_utf8(ok(&root, &["diff", "src-x"]).stdout).unwrap();
    assert!(human.contains("+src-x two"));
    assert!(!human.contains("src/a"));
    assert!(ok(&root, &["diff", "missing"]).stdout.is_empty());
}

#[test]
fn revision_diff_reports_only_selected_paths() {
    let (_parent, root, revision) = setup();
    let all = ["diff", "--revision", revision.as_str()];
    assert_eq!(diffed_paths(&root, &all).len(), 4);
    assert_eq!(
        diffed_paths(&root, &["diff", "--revision", &revision, "src"]),
        ["src/a", "src/sub/b"]
    );
    let human =
        String::from_utf8(ok(&root, &["diff", "--revision", &revision, "docs/c"]).stdout).unwrap();
    assert!(human.contains("--- /dev/null\n+++ b/docs/c"));
    assert!(human.contains("+docs/c one"));
    assert!(!human.contains("src"));
}

#[test]
fn view_diff_reports_only_selected_paths() {
    let (_parent, root, _) = setup();
    let status: serde_json::Value =
        serde_json::from_slice(&ok(&root, &["status", "--format", "json"]).stdout).unwrap();
    let effective: wheat_core::ViewId =
        serde_json::from_value(status["result"]["effective_view"].clone()).unwrap();
    let effective = effective.to_string();
    let views = ["diff", "--from", "label:main", "--to", effective.as_str()];
    let all = diffed_paths(&root, &views);
    assert_eq!(all.len(), 5, "{all:?}");
    let mut selected = views.to_vec();
    selected.extend(["seed", "src/sub"]);
    assert_eq!(diffed_paths(&root, &selected), ["seed", "src/sub/b"]);
}

#[test]
fn diff_rejects_paths_outside_the_repository() {
    let (_parent, root, _) = setup();
    for path in ["../x", "/abs", "a//b", ""] {
        let output = wheat(&root, &["diff", path]);
        assert_eq!(output.status.code(), Some(2), "{path:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid diff path"),
            "{path:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
