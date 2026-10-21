//! A team sharing one Git branch: a bare Git remote, Bob's Git clone of it, and a Wheat
//! workspace imported from it.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub fn git(directory: Option<&Path>, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Bob Git")
        .env("GIT_AUTHOR_EMAIL", "bob@example.invalid")
        .env("GIT_COMMITTER_NAME", "Bob Git")
        .env("GIT_COMMITTER_EMAIL", "bob@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

pub struct Team {
    pub root: tempfile::TempDir,
    pub remote: PathBuf,
    pub workspace: PathBuf,
    pub bob: PathBuf,
}

impl Team {
    /// A bare Git remote with two commits on `main`, a Git clone of it for Bob, and a
    /// Wheat workspace imported from it.
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let bob = root.path().join("bob");
        let workspace = root.path().join("wheat");
        git(
            None,
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        git(
            None,
            &["clone", remote.to_str().unwrap(), bob.to_str().unwrap()],
        );
        git(Some(&bob), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(bob.join("a.txt"), "one\n").unwrap();
        git(Some(&bob), &["add", "a.txt"]);
        git(Some(&bob), &["commit", "-m", "first"]);
        std::fs::write(bob.join("a.txt"), "one\ntwo\n").unwrap();
        git(Some(&bob), &["commit", "-am", "second"]);
        git(Some(&bob), &["push", "origin", "main"]);
        let team = Self {
            root,
            remote,
            workspace,
            bob,
        };
        let workspace = team.workspace.to_str().unwrap().to_owned();
        team.wheat_ok(&[
            "import",
            team.remote.to_str().unwrap(),
            &workspace,
            "--branch",
            "refs/heads/main",
        ]);
        team
    }

    pub fn remote_url(&self) -> &str {
        self.remote.to_str().unwrap()
    }

    pub fn wheat(&self, arguments: &[&str]) -> Output {
        let config_home = self.root.path().join("wheat-config");
        std::fs::create_dir_all(config_home.join("wheat")).unwrap();
        std::fs::write(
            config_home.join("wheat/config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .arg("--workspace")
            .arg(&self.workspace)
            .args(arguments)
            .env("WHEAT_STORE_HOME", self.root.path().join("stores"))
            .env("XDG_CONFIG_HOME", config_home)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    pub fn wheat_ok(&self, arguments: &[&str]) -> String {
        let output = self.wheat(arguments);
        assert!(
            output.status.success(),
            "wheat {arguments:?} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn wheat_json(&self, arguments: &[&str]) -> serde_json::Value {
        let mut arguments = arguments.to_vec();
        arguments.insert(0, "--format=json");
        let document: serde_json::Value = serde_json::from_str(&self.wheat_ok(&arguments)).unwrap();
        document["result"].clone()
    }

    /// Runs a command expected to fail; returns its exit code and JSON error.
    pub fn wheat_error(&self, arguments: &[&str]) -> (Option<i32>, serde_json::Value) {
        let mut arguments = arguments.to_vec();
        arguments.insert(0, "--format=json");
        let output = self.wheat(&arguments);
        assert!(
            !output.status.success(),
            "wheat {arguments:?} succeeded:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code(), document["error"].clone())
    }

    /// Writes `path` in the Wheat workspace and captures it as a new change.
    pub fn wheat_change(&self, message: &str, path: &str, contents: &str) {
        std::fs::write(self.workspace.join(path), contents).unwrap();
        self.wheat_ok(&["change", "new", "--message", message]);
        self.wheat_ok(&["add", path]);
    }

    /// Bob writes `path` in his Git clone, commits it and pushes it to the remote.
    pub fn bob_commits(&self, message: &str, path: &str, contents: &str) -> String {
        git(Some(&self.bob), &["pull", "--ff-only", "origin", "main"]);
        std::fs::write(self.bob.join(path), contents).unwrap();
        git(Some(&self.bob), &["add", path]);
        git(Some(&self.bob), &["commit", "-m", message]);
        git(Some(&self.bob), &["push", "origin", "main"]);
        git(Some(&self.bob), &["rev-parse", "HEAD"])
    }

    pub fn push(&self) {
        self.wheat_ok(&["push", self.remote_url(), "--to", "refs/heads/main"]);
    }

    pub fn pull(&self) -> serde_json::Value {
        self.wheat_json(&["pull", self.remote_url()])
    }

    /// Change titles from `wheat log`, sorted.
    pub fn change_titles(&self) -> Vec<String> {
        let log = self.wheat_json(&["log"]);
        let mut titles = log["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["change"]["title"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        titles.sort();
        titles
    }

    /// Subjects on the remote's `main`, newest first, following first parents.
    pub fn remote_history(&self) -> Vec<String> {
        git(
            Some(&self.remote),
            &["log", "--first-parent", "--format=%s", "refs/heads/main"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    pub fn remote_main(&self) -> String {
        git(Some(&self.remote), &["rev-parse", "refs/heads/main"])
    }

    /// The store's Git projection, which `wheat push` creates.
    pub fn projection(&self) -> PathBuf {
        let mut projections = std::fs::read_dir(self.root.path().join("stores"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("projections/git.git"))
            .filter(|projection| projection.is_dir());
        let projection = projections.next().expect("a pushed store");
        assert!(projections.next().is_none(), "one store");
        projection
    }

    pub fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.workspace.join(path)).unwrap()
    }

    /// Asserts the workspace holds no uncaptured work.
    pub fn assert_clean(&self) {
        let status = self.wheat_json(&["status"]);
        assert_eq!(status["changes"], serde_json::json!([]), "{status:#}");
        let untracked = status["untracked"].as_array().cloned().unwrap_or_default();
        assert!(
            untracked.iter().all(|entry| entry["ignored"] == true),
            "{status:#}"
        );
        assert_eq!(status["authority_advanced"], false, "{status:#}");
    }
}
