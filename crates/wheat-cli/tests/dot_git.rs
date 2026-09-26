//! Writing files inside a `.git` directory: refused when nobody can answer,
//! allowed with `--allow-dot-git`, and a refused clone can be finished later.
//! The test process has no terminal, so every command here is non-interactive.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const HOOK: &str = ".git/hooks/post-checkout";
const HOOK_BYTES: &str = "#!/bin/sh\necho planted\n";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// A workspace `source` whose tree holds a git hook and a plain file.
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let config = fixture.path("config/wheat");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();

        let source = fixture.path("source");
        fixture.ok(&["init", source.to_str().unwrap()]);
        fs::create_dir_all(source.join(".git/hooks")).unwrap();
        fs::write(source.join(HOOK), HOOK_BYTES).unwrap();
        fs::write(source.join("README"), "hello\n").unwrap();
        fixture.ok_in(&source, &["change", "new", "-m", "hook"]);
        fixture.ok_in(&source, &["add", HOOK, "README"]);
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn wheat_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .current_dir(cwd)
            .args(args)
            .env("WHEAT_STORE_HOME", self.path("stores"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .output()
            .unwrap()
    }

    fn wheat(&self, args: &[&str]) -> Output {
        self.wheat_in(self.root.path(), args)
    }

    fn ok_in(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = self.wheat_in(cwd, args);
        assert!(
            output.status.success(),
            "wheat {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn ok(&self, args: &[&str]) -> Output {
        self.ok_in(self.root.path(), args)
    }
}

fn refused_json(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "dot_git_paths", "{value}");
    value["error"]["message"].as_str().unwrap().to_owned()
}

#[test]
fn clone_without_the_flag_sets_up_the_workspace_but_writes_nothing() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    let clone = fixture.path("clone");

    let refused = fixture.wheat(&[
        "--format=json",
        "clone",
        source.to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    let message = refused_json(&refused);
    assert!(message.contains(HOOK), "{message}");
    assert!(!message.contains("README"), "{message}");
    assert!(message.contains("materialize --allow-dot-git"), "{message}");
    assert!(clone.join(".wheat").is_dir());
    assert!(!clone.join(HOOK).exists());
    assert!(!clone.join("README").exists(), "nothing is written");

    fixture.ok(&[
        "--workspace",
        clone.to_str().unwrap(),
        "materialize",
        "--allow-dot-git",
    ]);
    assert_eq!(fs::read_to_string(clone.join(HOOK)).unwrap(), HOOK_BYTES);
    assert_eq!(fs::read_to_string(clone.join("README")).unwrap(), "hello\n");
}

#[test]
fn clone_with_the_flag_writes_dot_git_paths() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    let clone = fixture.path("clone");

    fixture.ok(&[
        "clone",
        source.to_str().unwrap(),
        clone.to_str().unwrap(),
        "--allow-dot-git",
    ]);
    assert_eq!(fs::read_to_string(clone.join(HOOK)).unwrap(), HOOK_BYTES);
}

#[test]
fn human_output_without_a_terminal_refuses_and_names_the_flag() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    let clone = fixture.path("clone");

    let refused = fixture.wheat(&["clone", source.to_str().unwrap(), clone.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains(HOOK), "{stderr}");
    assert!(stderr.contains("--allow-dot-git"), "{stderr}");
    assert!(!stderr.contains("[y/N]"), "nobody is asked: {stderr}");
    assert!(!clone.join(HOOK).exists());
}

#[test]
fn restore_asks_only_about_dot_git_paths() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    fs::remove_file(source.join(HOOK)).unwrap();
    fs::remove_file(source.join("README")).unwrap();

    fixture.ok_in(&source, &["restore", "README"]);
    assert_eq!(
        fs::read_to_string(source.join("README")).unwrap(),
        "hello\n"
    );

    let refused = fixture.wheat_in(&source, &["--format=json", "restore", HOOK]);
    refused_json(&refused);
    assert!(!source.join(HOOK).exists());

    fixture.ok_in(&source, &["restore", "--allow-dot-git", HOOK]);
    assert_eq!(fs::read_to_string(source.join(HOOK)).unwrap(), HOOK_BYTES);
}

#[test]
fn materialize_that_writes_nothing_under_dot_git_does_not_refuse() {
    let fixture = Fixture::new();
    let source = fixture.path("source");
    // Everything is already in place, so there is nothing to write.
    fixture.ok_in(&source, &["materialize"]);
}
