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

#[test]
fn compares_initial_and_successor_revisions_without_workspace_reads() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    ok(&root, &["init"]);
    let initial = ok(
        &root,
        &["change", "new", "--path", "file", "--value", "one\n"],
    );
    let change = field(&initial, "change");
    let initial_revision = field(&initial, "revision");

    // The immutable command succeeds even though the workspace was never materialized.
    let initial_diff = ok(&root, &["diff", "--revision", &initial_revision]);
    let rendered = String::from_utf8_lossy(&initial_diff.stdout);
    assert!(rendered.contains("diff --wheat a/file b/file"));
    assert!(rendered.contains("--- /dev/null\n+++ b/file"));
    assert!(rendered.contains("@@ -0,0 +1,1 @@"));
    assert!(rendered.contains("+one"));
    assert!(!root.join("file").exists());

    // Materialize before authoring a successor from the selected workspace path.
    ok(&root, &["materialize", "--source", "working"]);
    std::fs::write(root.join("file"), "two\n").unwrap();
    let successor = ok(
        &root,
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &initial_revision,
            "file",
        ],
    );
    let successor_diff = ok(
        &root,
        &["diff", "--revision", &field(&successor, "revision")],
    );
    let rendered = String::from_utf8_lossy(&successor_diff.stdout);
    assert!(rendered.contains("@@ -1,1 +1,1 @@"));
    assert!(rendered.contains("-one"));
    assert!(rendered.contains("+two"));
}

#[test]
fn immutable_diff_classifies_utf8_with_nul_as_binary_and_emits_complete_json() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    ok(&root, &["init"]);
    let created = ok(
        &root,
        &["change", "new", "--path", "nul", "--value", "text"],
    );
    let change = field(&created, "change");
    let predecessor = field(&created, "revision");
    ok(&root, &["materialize", "--source", "working"]);
    std::fs::write(root.join("nul"), b"left\0right").unwrap();
    let updated = ok(
        &root,
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "nul",
        ],
    );
    let revision = field(&updated, "revision");

    let human = ok(&root, &["diff", "--revision", &revision]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("Binary files differ"));
    let json = ok(
        &root,
        &["diff", "--revision", &revision, "--format", "json"],
    );
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(document["files"][0]["change"], "modified");
    assert_eq!(document["files"][0]["presentation"]["type"], "binary");
}

#[test]
fn compares_symbolic_and_exact_retained_views_and_rejects_unknown_ids() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    ok(&root, &["init"]);
    ok(
        &root,
        &["change", "new", "--path", "file", "--value", "value"],
    );

    let status: serde_json::Value =
        serde_json::from_slice(&ok(&root, &["status", "--format", "json"]).stdout).unwrap();
    let published: wheat_core::ViewId =
        serde_json::from_value(status["published_view"].clone()).unwrap();
    let working: wheat_core::ViewId =
        serde_json::from_value(status["working_view"].clone()).unwrap();
    let published = published.to_string();
    let working = working.to_string();
    let symbolic = ok(&root, &["diff", "--from", "published", "--to", "working"]);
    let exact = ok(&root, &["diff", "--from", &published, "--to", &working]);
    assert_eq!(symbolic.stdout, exact.stdout);

    let unknown = wheat(
        &root,
        &[
            "diff",
            "--from",
            &wheat_core::ViewId::from_digest([0x55; 32]).to_string(),
            "--to",
            "working",
        ],
    );
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("not retained"));
}
