#![cfg(target_os = "linux")]

//! Integration tests for `wheat add --all` workspace-wide capture.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

struct Harness {
    root: tempfile::TempDir,
    workspace: PathBuf,
    stores: PathBuf,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        Self {
            workspace: root.path().join("workspace"),
            stores: root.path().join("stores"),
            config: root.path().join("config"),
            root,
        }
    }

    fn wheat(&self, current_dir: &Path, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .current_dir(current_dir)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("wheat command runs in its own process")
    }

    fn ok(&self, current_dir: &Path, arguments: &[&str]) -> Output {
        let output = self.wheat(current_dir, arguments);
        assert!(
            output.status.success(),
            "wheat {arguments:?} failed (status {:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    fn json_ok(&self, current_dir: &Path, command_arguments: &[&str]) -> Value {
        let mut arguments = vec!["--format=json"];
        arguments.extend_from_slice(command_arguments);
        let output = self.ok(current_dir, &arguments);
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["format"], "wheat-command-result");
        assert_eq!(document["ok"], true);
        document
    }

    fn init_from_git(&self, files: &[(&str, &str)]) {
        let git_dir = self.root.path().join("source.git");
        fs::create_dir_all(&git_dir).unwrap();
        git(Some(&git_dir), &["init", "--bare"]);

        let work = self.root.path().join("source-work");
        git(
            None,
            &["clone", git_dir.to_str().unwrap(), work.to_str().unwrap()],
        );
        for (path, content) in files {
            let full_path = work.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full_path, content).unwrap();
        }
        git(Some(&work), &["add", "-A"]);
        git(Some(&work), &["commit", "-m", "initial"]);
        git(Some(&work), &["push", "origin", "HEAD:refs/heads/main"]);

        // Import materializes the workspace as part of its flow.
        self.ok(
            self.root.path(),
            &[
                "import",
                git_dir.to_str().unwrap(),
                self.workspace.to_str().unwrap(),
            ],
        );

        let config_dir = self.config.join("wheat");
        fs::create_dir_all(&config_dir).unwrap();
        let config = r#"{"format":"wheat-config","version":2,"actor":{"name":"Test","email":"test@example.com"}}"#;
        fs::write(config_dir.join("config.json"), config).unwrap();
    }
}

fn git(directory: Option<&Path>, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git command runs");
    assert!(output.status.success(), "git {arguments:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn add_all_captures_modified_tracked_paths() {
    let h = Harness::new();
    h.init_from_git(&[
        ("src/main.rs", "fn main() {}\n"),
        ("src/lib.rs", "pub fn hello() {}\n"),
        ("README.md", "# Project\n"),
    ]);

    // Create a logical change.
    h.ok(&h.workspace, &["change", "new", "-m", "update files"]);

    // Modify two tracked files, leave one unchanged.
    fs::write(
        h.workspace.join("src/main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    fs::write(h.workspace.join("README.md"), "# Updated Project\n").unwrap();

    // Add --all should capture the two modified files.
    let result = h.json_ok(&h.workspace, &["add", "--all"]);
    assert_eq!(result["result"]["paths"], 2);
    assert!(result["result"]["revision"].is_string());

    // Verify the revision was created with both paths by checking the
    // change is still active with a new revision ID.
    let changes = h.json_ok(&h.workspace, &["changes"]);
    assert_eq!(
        changes["result"]["changes"].as_array().unwrap().len(),
        1,
        "one active change should exist"
    );
}

#[test]
fn add_all_rejects_clean_workspace() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);
    h.ok(&h.workspace, &["change", "new", "-m", "nothing to do"]);

    // No modifications — --all should fail.
    let output = h.wheat(&h.workspace, &["add", "--all"]);
    assert!(
        !output.status.success(),
        "add --all on a clean workspace should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = format!("{stderr}{stdout}");
    assert!(
        message.contains("clean") || message.contains("no modified"),
        "error should mention clean workspace; got: {message}"
    );
}

#[test]
fn add_all_and_explicit_paths_are_mutually_exclusive() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    let output = h.wheat(&h.workspace, &["add", "--all", "tracked.txt"]);
    assert!(
        !output.status.success(),
        "add --all with explicit paths should fail"
    );
}

#[test]
fn add_all_excludes_untracked_files() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);
    h.ok(&h.workspace, &["change", "new", "-m", "modify tracked"]);

    // Modify tracked file and create an untracked file.
    fs::write(h.workspace.join("tracked.txt"), "modified\n").unwrap();
    fs::write(h.workspace.join("new-file.txt"), "untracked\n").unwrap();

    // Add --all captures only the tracked modification.
    let result = h.json_ok(&h.workspace, &["add", "--all"]);
    assert_eq!(result["result"]["paths"], 1);

    // The untracked file should still show up in status.
    let status = h.json_ok(&h.workspace, &["status"]);
    let empty = Vec::new();
    let untracked: Vec<&str> = status["result"]["untracked"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|e| !e["ignored"].as_bool().unwrap_or(false))
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        untracked.contains(&"new-file.txt"),
        "untracked file should remain after add --all"
    );
}

#[test]
fn add_all_captures_deleted_tracked_paths() {
    let h = Harness::new();
    h.init_from_git(&[("keep.txt", "keep\n"), ("remove.txt", "will be deleted\n")]);
    h.ok(&h.workspace, &["change", "new", "-m", "delete a file"]);

    // Delete a tracked file.
    fs::remove_file(h.workspace.join("remove.txt")).unwrap();

    // Add --all should capture the deletion.
    let result = h.json_ok(&h.workspace, &["add", "--all"]);
    assert_eq!(result["result"]["paths"], 1);
}

#[test]
fn add_without_all_or_paths_fails() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    let output = h.wheat(&h.workspace, &["add"]);
    assert!(
        !output.status.success(),
        "add with no paths and no --all should fail"
    );
}
