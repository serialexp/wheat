#![cfg(target_os = "linux")]
//! `add` settles the materialized pin for captured workspace bytes, and `materialize`
//! accepts workspace leaves that already hold their exact target value.

use std::{
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

struct Harness {
    _root: tempfile::TempDir,
    stores: PathBuf,
    config: PathBuf,
    parent: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let stores = root.path().join("stores");
        let config = root.path().join("config");
        fs::create_dir_all(config.join("wheat")).unwrap();
        fs::write(
            config.join("wheat/config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();
        let parent = root.path().to_path_buf();
        Self {
            _root: root,
            stores,
            config,
            parent,
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.parent.join(name)
    }

    fn wheat(&self, workspace: &Path, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .arg("--workspace")
            .arg(workspace)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config)
            .output()
            .unwrap()
    }

    fn ok(&self, workspace: &Path, arguments: &[&str]) -> Output {
        let output = self.wheat(workspace, arguments);
        assert!(
            output.status.success(),
            "command {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, workspace: &Path, arguments: &[&str]) -> Value {
        let mut all = vec!["--format", "json"];
        all.extend_from_slice(arguments);
        let document: Value = serde_json::from_slice(&self.ok(workspace, &all).stdout).unwrap();
        assert_eq!(document["ok"], true);
        document["result"].clone()
    }

    fn init(&self, name: &str) -> PathBuf {
        let workspace = self.workspace(name);
        fs::create_dir_all(&workspace).unwrap();
        self.ok(&self.parent, &["init", workspace.to_str().unwrap()]);
        workspace
    }

    fn new_change(&self, workspace: &Path, arguments: &[&str]) -> String {
        let mut all = vec!["change", "new"];
        all.extend_from_slice(arguments);
        self.json(workspace, &all)["change"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn add(&self, workspace: &Path, arguments: &[&str]) -> Value {
        let mut all = vec!["add", "--message", "capture"];
        all.extend_from_slice(arguments);
        self.json(workspace, &all)
    }

    fn assert_clean(&self, workspace: &Path) {
        let status = self.json(workspace, &["status"]);
        assert_eq!(status["authority_advanced"], false, "{status}");
        assert_eq!(status["changes"], serde_json::json!([]), "{status}");
        let untracked = status["untracked"].as_array().unwrap();
        assert!(
            untracked.iter().all(|entry| entry["ignored"] == true),
            "{status}"
        );
    }
}

fn ino(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}

#[test]
fn add_in_a_populated_new_workspace_tracks_captured_files_without_materializing() {
    let harness = Harness::new();
    let workspace = harness.init("workspace");
    fs::write(workspace.join("a.txt"), b"alpha\n").unwrap();
    fs::create_dir(workspace.join("nested")).unwrap();
    fs::write(workspace.join("nested/b.txt"), b"beta\n").unwrap();
    let inode = ino(&workspace.join("a.txt"));

    harness.new_change(&workspace, &["--message", "Initial import"]);
    let added = harness.add(&workspace, &["a.txt", "nested/b.txt"]);
    assert_eq!(added["materialize_required"], false);
    assert_eq!(added["diverged"], serde_json::json!([]));
    harness.assert_clean(&workspace);

    // Materialization has nothing left to install and leaves the captured files alone.
    harness.ok(&workspace, &["materialize"]);
    assert_eq!(ino(&workspace.join("a.txt")), inode);
    harness.assert_clean(&workspace);

    // Later captures of modified and deleted tracked paths settle the same way.
    fs::write(workspace.join("a.txt"), b"alpha revised\n").unwrap();
    fs::remove_file(workspace.join("nested/b.txt")).unwrap();
    fs::remove_dir(workspace.join("nested")).unwrap();
    let added = harness.add(&workspace, &["a.txt", "nested/b.txt"]);
    assert_eq!(added["materialize_required"], false);
    harness.assert_clean(&workspace);
    let human = String::from_utf8(harness.ok(&workspace, &["status"]).stdout).unwrap();
    assert!(!human.contains("repository advanced since materialization"));
}

#[test]
fn add_never_writes_unmaterialized_view_content_and_reports_the_lineage_change() {
    let harness = Harness::new();
    let workspace = harness.init("workspace");
    let ancestor = harness.new_change(&workspace, &["--message", "Ancestor"]);
    fs::write(workspace.join("mine.txt"), b"mine\n").unwrap();
    harness.add(&workspace, &["mine.txt"]);
    harness.assert_clean(&workspace);

    // A descendant whose content this workspace has never materialized.
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Descendant",
            "--path",
            "other.txt",
            "--value",
            "other\n",
        ],
    );
    assert!(!workspace.join("other.txt").exists());

    fs::write(workspace.join("mine.txt"), b"mine revised\n").unwrap();
    let revised = harness.add(&workspace, &["--change", &ancestor, "mine.txt"]);
    assert_eq!(revised["reapplied"].as_array().unwrap().len(), 1);
    assert_eq!(revised["materialize_required"], true);
    assert_eq!(revised["diverged"], serde_json::json!([]));
    // The descendant is not written into the workspace behind the author's back.
    assert!(!workspace.join("other.txt").exists());
    assert_eq!(
        fs::read(workspace.join("mine.txt")).unwrap(),
        b"mine revised\n"
    );
    // The captured path is settled; only the unmaterialized lineage is outstanding.
    let status = harness.json(&workspace, &["status"]);
    assert_eq!(status["authority_advanced"], true);
    assert_eq!(status["changes"], serde_json::json!([]));

    // Human output states the outstanding materialization for every capture until it runs.
    fs::write(workspace.join("mine.txt"), b"mine revised twice\n").unwrap();
    let human = harness.ok(
        &workspace,
        &[
            "add",
            "--message",
            "again",
            "--change",
            &ancestor,
            "mine.txt",
        ],
    );
    assert!(
        String::from_utf8_lossy(&human.stdout)
            .contains("lineage changed: the working view includes changes this workspace has not materialized; run `wheat materialize` to update it")
    );
    assert!(!workspace.join("other.txt").exists());

    let inode = ino(&workspace.join("mine.txt"));
    harness.ok(&workspace, &["materialize"]);
    assert_eq!(fs::read(workspace.join("other.txt")).unwrap(), b"other\n");
    assert_eq!(ino(&workspace.join("mine.txt")), inode);
    harness.assert_clean(&workspace);
}

#[test]
fn a_captured_path_a_reapplied_descendant_also_edits_stays_materializable() {
    let harness = Harness::new();
    let workspace = harness.init("workspace");
    let ancestor = harness.new_change(&workspace, &["--message", "Ancestor"]);
    fs::write(workspace.join("f"), b"1\n2\n3\n4\n5\n").unwrap();
    harness.add(&workspace, &["f"]);
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Descendant",
            "--path",
            "f",
            "--expected",
            "1\n2\n3\n4\n5\n",
            "--value",
            "1\n2\n3\n4\nD\n",
        ],
    );

    fs::write(workspace.join("f"), b"A\n2\n3\n4\n5\n").unwrap();
    let revised = harness.add(&workspace, &["--change", &ancestor, "f"]);
    assert_eq!(revised["materialize_required"], true);
    assert_eq!(revised["diverged"], serde_json::json!(["f"]));
    assert_eq!(fs::read(workspace.join("f")).unwrap(), b"A\n2\n3\n4\n5\n");
    // The pin records the captured change's own value, which the workspace holds.
    let status = harness.json(&workspace, &["status"]);
    assert_eq!(status["changes"], serde_json::json!([]));

    // Materialization authenticates the captured bytes and installs the descendant's result.
    harness.ok(&workspace, &["materialize"]);
    assert_eq!(fs::read(workspace.join("f")).unwrap(), b"A\n2\n3\n4\nD\n");
    harness.assert_clean(&workspace);
}

#[test]
fn another_workspace_revising_a_shared_ancestor_is_not_pulled_in_by_add() {
    let harness = Harness::new();
    let first = harness.init("first");
    let ancestor = harness.new_change(
        &first,
        &[
            "--message",
            "Shared ancestor",
            "--path",
            "base.txt",
            "--value",
            "base\n",
        ],
    );
    harness.ok(&first, &["materialize"]);
    let second = harness.workspace("second");
    harness.ok(
        &harness.parent,
        &["clone", first.to_str().unwrap(), second.to_str().unwrap()],
    );

    harness.new_change(&first, &["--message", "Mine"]);
    fs::write(first.join("mine.txt"), b"mine\n").unwrap();
    harness.add(&first, &["mine.txt"]);
    harness.assert_clean(&first);

    // Someone else revises the ancestor this workspace builds on.
    fs::write(second.join("base.txt"), b"base revised elsewhere\n").unwrap();
    fs::write(second.join("theirs.txt"), b"theirs\n").unwrap();
    harness.add(&second, &["--change", &ancestor, "base.txt", "theirs.txt"]);

    fs::write(first.join("mine.txt"), b"mine revised\n").unwrap();
    let added = harness.add(&first, &["mine.txt"]);
    assert_eq!(added["materialize_required"], false);
    assert_eq!(fs::read(first.join("base.txt")).unwrap(), b"base\n");
    assert!(!first.join("theirs.txt").exists());
    harness.assert_clean(&first);
}

#[test]
fn materialize_accepts_leaves_that_already_hold_their_target() {
    let harness = Harness::new();
    let workspace = harness.init("workspace");
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Doomed",
            "--path",
            "doomed.txt",
            "--value",
            "doomed\n",
        ],
    );
    harness.ok(&workspace, &["materialize"]);

    harness.new_change(
        &workspace,
        &[
            "--message",
            "Delete",
            "--path",
            "doomed.txt",
            "--expected",
            "doomed\n",
            "--delete",
        ],
    );
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Present",
            "--path",
            "present.txt",
            "--value",
            "present\n",
        ],
    );
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Absent",
            "--path",
            "absent.txt",
            "--value",
            "absent\n",
        ],
    );
    // The workspace already reflects two of the three pending operations.
    fs::remove_file(workspace.join("doomed.txt")).unwrap();
    fs::write(workspace.join("present.txt"), b"present\n").unwrap();
    let inode = ino(&workspace.join("present.txt"));

    harness.ok(&workspace, &["materialize"]);
    assert!(!workspace.join("doomed.txt").exists());
    assert_eq!(ino(&workspace.join("present.txt")), inode);
    assert_eq!(fs::read(workspace.join("absent.txt")).unwrap(), b"absent\n");
    harness.assert_clean(&workspace);
}

#[test]
fn materialize_still_refuses_leaves_that_hold_neither_pinned_nor_target_bytes() {
    let harness = Harness::new();
    let workspace = harness.init("workspace");
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Create",
            "--path",
            "file.txt",
            "--value",
            "target\n",
        ],
    );
    fs::write(workspace.join("file.txt"), b"local work\n").unwrap();
    harness.new_change(
        &workspace,
        &[
            "--message",
            "Other",
            "--path",
            "other.txt",
            "--value",
            "other\n",
        ],
    );

    let refused = harness.wheat(&workspace, &["materialize"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("changed from its exact observed value"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("file.txt")).unwrap(),
        b"local work\n"
    );
    assert!(!workspace.join("other.txt").exists());
}
