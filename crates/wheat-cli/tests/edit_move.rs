#![cfg(target_os = "linux")]
//! `wheat add --change B` places the edit at B's spot in the stack.
//!
//! The workspace shows every change applied, so a file can hold bytes that changes after B
//! wrote. An edit on B's own bytes goes into B and leaves the later changes alone. An edit that
//! overlaps a later change's block moves that whole block into B, and a file a later change
//! created moves entirely; both need confirmation, or `--force` without a terminal.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

/// Diffs here must come out whole however busy the machine is.
const DIFF_TIME_LIMIT: &str = "--time-limit-ms=600000";

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let config = dir.path().join("config").join("wheat");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();
        let workspace = Self { _dir: dir, root };
        workspace.ok(&["init", workspace.root.to_str().unwrap()]);
        workspace
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let home = self.root.parent().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
        command
            .arg("--workspace")
            .arg(&self.root)
            .args(arguments)
            .env("WHEAT_STORE_HOME", home.join("stores"))
            .env("XDG_CONFIG_HOME", home.join("config"));
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments)
            .output()
            .expect("wheat command runs")
    }

    fn ok(&self, arguments: &[&str]) -> Output {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "command {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.root.join(path), contents).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    /// The workspace matches the working view, the repository validates, and a fresh
    /// materialization reproduces `expected` for each path.
    fn settled(&self, expected: &[(&str, &str)]) {
        let status = self.ok(&["status"]);
        assert!(
            String::from_utf8_lossy(&status.stdout).contains("tracked changes: 0"),
            "workspace should be clean: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        self.ok(&["validate"]);
        self.ok(&["materialize"]);
        for (path, contents) in expected {
            assert_eq!(self.read(path), *contents, "{path}");
        }
    }
}

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| {
            panic!(
                "no {name:?} line in {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
        .to_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Twenty lines, `l1` to `l20`.
fn base() -> String {
    (1..=20).map(|n| format!("l{n}\n")).collect()
}

/// The file after C rewrote line 10 into two lines. C's edit is guarded by the two lines on
/// either side of it.
fn after_c() -> String {
    base().replace("l10\n", "C1\nC2\n")
}

/// Change B creates `file`, then change C rewrites its tenth line into two.
fn stack() -> (Workspace, String, String) {
    let workspace = Workspace::new();
    let b = field(&workspace.ok(&["change", "new", "-m", "B"]), "change");
    workspace.write("file", &base());
    workspace.ok(&["add", "-m", "B1", "file"]);
    let c = field(&workspace.ok(&["change", "new", "-m", "C"]), "change");
    workspace.write("file", &after_c());
    let c1 = field(&workspace.ok(&["add", "-m", "C1", "file"]), "revision");
    assert!(edits(&workspace, &c1).contains("file"));
    (workspace, b, c)
}

/// The `wheat edits` listing of `revision`.
fn edits(workspace: &Workspace, revision: &str) -> String {
    stdout(&workspace.ok(&["edits", "--revision", revision]))
}

/// B's own delta in `revision`.
fn revision_diff(workspace: &Workspace, revision: &str) -> String {
    stdout(&workspace.ok(&["diff", DIFF_TIME_LIMIT, "--revision", revision, "file"]))
}

#[test]
fn an_edit_on_the_earlier_changes_lines_leaves_later_edits_alone() {
    let (workspace, b, c) = stack();
    let edited = after_c().replace("l1\n", "L1\n").replace("l20\n", "L20\n");
    let edited = edited.as_str();
    workspace.write("file", edited);
    let added = workspace.ok(&["add", "-m", "B2", "--change", &b, "file"]);
    let out = stdout(&added);
    assert!(!out.contains("moved"), "{out}");
    assert!(out.contains(&format!("reapplied {c}")), "{out}");

    // B took only the author's two lines; C still owns its rewrite of line three.
    let diff = revision_diff(&workspace, &field(&added, "revision"));
    assert!(diff.contains("+L1\n") && diff.contains("+L20\n"), "{diff}");
    assert!(!diff.contains("C1"), "B must not swallow C's edit: {diff}");
    workspace.settled(&[("file", edited)]);
}

#[test]
fn an_edit_overlapping_a_later_block_asks_before_moving_it() {
    let (workspace, b, c) = stack();
    let edited = after_c().replace("C2\n", "X2\n");
    let edited = edited.as_str();
    workspace.write("file", edited);

    // Without a terminal the add lists the move and refuses.
    let refused = workspace.run(&["add", "-m", "B2", "--change", &b, "file"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = stderr(&refused);
    assert!(
        message.contains(&format!(
            "file: your edit on line 11 overlaps change {c}'s edit on lines 10-11; that block moves into {b} wholesale"
        )),
        "{message}"
    );
    assert!(message.contains("rerun with --force"), "{message}");

    let refused = workspace.run(&[
        "add", "-m", "B2", "--change", &b, "file", "--format", "json",
    ]);
    assert_eq!(refused.status.code(), Some(2));
    let document: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(document["error"]["code"], "edits_would_move");

    // Nothing changed: the workspace still holds the unadded edit.
    let status = stdout(&workspace.ok(&["status"]));
    assert!(status.contains("tracked changes: 1"), "{status}");

    let added = workspace.ok(&[
        "add", "-m", "B2", "--change", &b, "file", "--force", "--format", "json",
    ]);
    let document: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let moved = document["result"]["moved"].as_array().unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0]["from"], c.as_str());
    assert_eq!(moved[0]["path"], "file");

    // B now owns the whole block, and C's revision no longer touches the file.
    let revision = document["result"]["revision"].as_str().unwrap();
    let diff = revision_diff(&workspace, revision);
    assert!(diff.contains("+C1\n") && diff.contains("+X2\n"), "{diff}");
    let c_revision = moved[0]["revision"].as_str().unwrap();
    let c_edits = edits(&workspace, c_revision);
    assert!(!c_edits.contains("file"), "{c_edits}");
    workspace.settled(&[("file", edited)]);
}

#[test]
fn a_file_a_later_change_created_moves_entirely() {
    let (workspace, b, c) = stack();
    workspace.write("new", "one\n");
    workspace.ok(&["add", "-m", "C2", "new"]);
    workspace.write("new", "two\n");

    let refused = workspace.run(&["add", "-m", "B2", "--change", &b, "new"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = stderr(&refused);
    assert!(
        message.contains(&format!(
            "new: the file doesn't exist in change {b}; change {c} created it; adding it to {b} moves the entire file into {b}"
        )),
        "{message}"
    );

    let added = workspace.ok(&["add", "-m", "B2", "--change", &b, "new", "--force"]);
    let out = stdout(&added);
    assert!(out.contains("moved new: the file doesn't exist"), "{out}");
    workspace.settled(&[("new", "two\n"), ("file", &after_c())]);
}

#[test]
fn a_later_edit_whose_context_changes_moves_too() {
    let (workspace, b, c) = stack();
    // Line nine is B's own, but C's edit of line ten is guarded by the lines before it.
    let edited = after_c().replace("l9\n", "L9\n");
    let edited = edited.as_str();
    workspace.write("file", edited);
    let refused = workspace.run(&["add", "-m", "B2", "--change", &b, "file"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = stderr(&refused);
    assert!(
        message.contains(&format!(
            "file: change {c}'s edit on lines 10-11 no longer fits after your edit; it moves into {b} too"
        )),
        "{message}"
    );
    workspace.ok(&["add", "-m", "B2", "--change", &b, "file", "--force"]);
    workspace.settled(&[("file", edited)]);
}

#[test]
fn only_the_later_changes_the_workspace_shows_are_moved_from() {
    let (workspace, b, c) = stack();
    // D edits the last line after the workspace was materialized, so the author never saw it.
    let after_d = after_c().replace("l20\n", "D20\n");
    let d = field(
        &workspace.ok(&[
            "change",
            "new",
            "-m",
            "D",
            "--path",
            "file",
            "--expected",
            &after_c(),
            "--value",
            &after_d,
        ]),
        "change",
    );
    let edited = after_c().replace("C2\n", "X2\n");
    workspace.write("file", &edited);

    let added = workspace.ok(&[
        "add", "-m", "B2", "--change", &b, "file", "--force", "--format", "json",
    ]);
    let document: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let moved = document["result"]["moved"].as_array().unwrap();
    assert_eq!(moved.len(), 1, "{document}");
    assert_eq!(moved[0]["from"], c.as_str());
    assert!(!document.to_string().contains(&format!("\"from\":\"{d}\"")));

    // D's edit stays in D and still applies on top.
    workspace.settled(&[("file", &edited.replace("l20\n", "D20\n"))]);
}

// The deep-stack scale benchmark is the `deep_stack` case of `wheat-bench quick`.

#[test]
fn adding_to_the_top_change_never_moves_anything() {
    let (workspace, _, c) = stack();
    let edited = after_c().replace("C2\n", "X2\n");
    let edited = edited.as_str();
    workspace.write("file", edited);
    let added = workspace.ok(&["add", "-m", "C2", "--change", &c, "file"]);
    assert!(!stdout(&added).contains("moved"));
    workspace.settled(&[("file", edited)]);
}
