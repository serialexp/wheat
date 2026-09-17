#![cfg(target_os = "linux")]

//! Integration tests for workspace-wide status with ignore rules.
//!
//! Exercises `.wheatignore` loading, `.gitignore` fallback with diagnostic,
//! directory pruning, negation patterns, global ignore, tracked-path
//! visibility, and symlink non-traversal.

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Harness — reusable test infrastructure
// ---------------------------------------------------------------------------

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
        // Allow stderr — the ignore fallback prints diagnostics there.
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["format"], "wheat-command-result");
        assert_eq!(document["ok"], true);
        document
    }

    fn stderr(&self, current_dir: &Path, command_arguments: &[&str]) -> String {
        let output = self.ok(current_dir, command_arguments);
        String::from_utf8_lossy(&output.stderr).to_string()
    }

    /// Set up a minimal Git repository, import it, materialize, and configure
    /// the actor identity.
    fn init_from_git(&self, files: &[(&str, &str)]) {
        let git_dir = self.root.path().join("source.git");
        fs::create_dir_all(&git_dir).unwrap();
        git(Some(&git_dir), &["init", "--bare"]);

        let work = self.root.path().join("source-work");
        git(None, &["clone", git_dir.to_str().unwrap(), work.to_str().unwrap()]);
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

        self.ok(
            self.root.path(),
            &[
                "import",
                git_dir.to_str().unwrap(),
                self.workspace.to_str().unwrap(),
            ],
        );
        self.ok(&self.workspace, &["materialize"]);

        // Configure actor identity.
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
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn untracked_paths(status: &Value) -> Vec<String> {
    status["result"]["untracked"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|e| !e["ignored"].as_bool().unwrap_or(false))
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect()
}

fn ignored_paths(status: &Value) -> Vec<String> {
    status["result"]["untracked"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|e| e["ignored"].as_bool().unwrap_or(false))
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn untracked_files_appear_in_status() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Create an untracked file.
    fs::write(h.workspace.join("new-file.txt"), "untracked\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let untracked = untracked_paths(&status);
    assert!(
        untracked.contains(&"new-file.txt".to_owned()),
        "new-file.txt should be untracked; got: {untracked:?}"
    );
}

#[test]
fn wheatignore_suppresses_untracked_files() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Create untracked files and a .wheatignore.
    fs::write(h.workspace.join("output.log"), "log data\n").unwrap();
    fs::write(h.workspace.join("keep.txt"), "keep\n").unwrap();
    fs::write(h.workspace.join(".wheatignore"), "*.log\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let untracked = untracked_paths(&status);
    let ignored = ignored_paths(&status);

    assert!(
        !untracked.contains(&"output.log".to_owned()),
        "output.log should be ignored, not untracked"
    );
    assert!(
        ignored.contains(&"output.log".to_owned()),
        "output.log should appear as ignored"
    );
    assert!(
        untracked.contains(&"keep.txt".to_owned()),
        "keep.txt should be untracked"
    );
}

#[test]
fn gitignore_fallback_with_diagnostic() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // The imported Git repo may have a .gitignore; create one if not.
    fs::write(h.workspace.join(".gitignore"), "*.log\n").unwrap();
    fs::write(h.workspace.join("debug.log"), "debug\n").unwrap();
    fs::write(h.workspace.join("keep.txt"), "keep\n").unwrap();

    // Check the stderr diagnostic.
    let stderr = h.stderr(&h.workspace, &["status"]);
    assert!(
        stderr.contains(".gitignore") && stderr.contains("wheat ignore import"),
        "expected .gitignore fallback diagnostic; got stderr: {stderr}"
    );

    // Check the JSON status.
    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);
    assert!(
        ignored.contains(&"debug.log".to_owned()),
        "debug.log should be ignored via .gitignore fallback"
    );
}

#[test]
fn wheatignore_replaces_gitignore_in_same_directory() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Both .gitignore and .wheatignore exist; .wheatignore should win.
    fs::write(h.workspace.join(".gitignore"), "*.log\n").unwrap();
    fs::write(h.workspace.join(".wheatignore"), "*.tmp\n").unwrap();
    fs::write(h.workspace.join("debug.log"), "debug\n").unwrap();
    fs::write(h.workspace.join("scratch.tmp"), "tmp\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);
    let untracked = untracked_paths(&status);

    assert!(
        !ignored.contains(&"debug.log".to_owned()),
        "debug.log should NOT be ignored (.wheatignore replaces .gitignore)"
    );
    assert!(
        untracked.contains(&"debug.log".to_owned()),
        "debug.log should be untracked"
    );
    assert!(
        ignored.contains(&"scratch.tmp".to_owned()),
        "scratch.tmp should be ignored by .wheatignore"
    );

    // No .gitignore fallback diagnostic when .wheatignore exists.
    let stderr = h.stderr(&h.workspace, &["status"]);
    assert!(
        !stderr.contains("wheat ignore import"),
        "no fallback diagnostic when .wheatignore exists"
    );
}

#[test]
fn negation_pattern_un_ignores() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    fs::write(h.workspace.join(".wheatignore"), "*.txt\n!important.txt\n").unwrap();
    fs::write(h.workspace.join("notes.txt"), "notes\n").unwrap();
    fs::write(h.workspace.join("important.txt"), "important\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);
    let untracked = untracked_paths(&status);

    assert!(
        ignored.contains(&"notes.txt".to_owned()),
        "notes.txt should be ignored"
    );
    assert!(
        untracked.contains(&"important.txt".to_owned()),
        "important.txt should be un-ignored by negation"
    );
}

#[test]
fn directory_pruning_skips_ignored_directories() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Create an ignored directory with many files inside.
    fs::write(h.workspace.join(".wheatignore"), "node_modules/\n").unwrap();
    let nm = h.workspace.join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    for i in 0..100 {
        fs::write(nm.join(format!("package-{i}.js")), "module\n").unwrap();
    }

    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);

    // The directory itself should be reported as ignored.
    assert!(
        ignored.contains(&"node_modules".to_owned()),
        "node_modules should be ignored"
    );
    // None of the files inside should be individually listed.
    assert!(
        !ignored.iter().any(|p| p.starts_with("node_modules/")),
        "files inside ignored directory should not be listed individually"
    );
    let untracked = untracked_paths(&status);
    assert!(
        !untracked.iter().any(|p| p.starts_with("node_modules/")),
        "files inside ignored directory should not be untracked"
    );
}

#[test]
fn nested_wheatignore_overrides_parent() {
    let h = Harness::new();
    h.init_from_git(&[("src/main.rs", "fn main() {}\n")]);

    // Parent ignores all .txt files.
    fs::write(h.workspace.join(".wheatignore"), "*.txt\n").unwrap();
    // Nested dir un-ignores README.txt.
    let sub = h.workspace.join("docs");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join(".wheatignore"), "!README.txt\n").unwrap();
    fs::write(sub.join("notes.txt"), "notes\n").unwrap();
    fs::write(sub.join("README.txt"), "readme\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let untracked = untracked_paths(&status);
    let ignored = ignored_paths(&status);

    assert!(
        ignored.contains(&"docs/notes.txt".to_owned()),
        "docs/notes.txt should be ignored by parent rule"
    );
    assert!(
        untracked.contains(&"docs/README.txt".to_owned()),
        "docs/README.txt should be un-ignored by nested .wheatignore"
    );
}

#[test]
fn global_ignore_file() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Set up a global ignore file.
    let global_dir = h.config.join("wheat");
    fs::create_dir_all(&global_dir).unwrap();
    fs::write(global_dir.join("ignore"), "*.bak\n").unwrap();

    fs::write(h.workspace.join("backup.bak"), "backup\n").unwrap();
    fs::write(h.workspace.join("keep.dat"), "data\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);
    let untracked = untracked_paths(&status);

    assert!(
        ignored.contains(&"backup.bak".to_owned()),
        "backup.bak should be ignored by global ignore"
    );
    assert!(
        untracked.contains(&"keep.dat".to_owned()),
        "keep.dat should be untracked"
    );
}

#[test]
fn tracked_paths_remain_visible_despite_ignore_rule() {
    let h = Harness::new();
    // Import a .txt file as tracked content.
    h.init_from_git(&[("README.txt", "hello\n")]);

    // Add an ignore rule that matches the tracked file.
    fs::write(h.workspace.join(".wheatignore"), "*.txt\n").unwrap();
    // Modify the tracked file.
    fs::write(h.workspace.join("README.txt"), "modified\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let changes = &status["result"]["changes"];
    let changed_paths: Vec<&str> = changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();

    assert!(
        changed_paths.contains(&"README.txt"),
        "tracked file should appear in changes even though it matches ignore rule"
    );
    // It should NOT appear as untracked or ignored.
    let untracked = untracked_paths(&status);
    let ignored = ignored_paths(&status);
    assert!(!untracked.contains(&"README.txt".to_owned()));
    assert!(!ignored.contains(&"README.txt".to_owned()));
}

#[test]
fn symlink_to_directory_not_followed() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // Create a real directory with content, then symlink to it.
    let real_dir = h.root.path().join("external");
    fs::create_dir_all(&real_dir).unwrap();
    fs::write(real_dir.join("secret.txt"), "secret\n").unwrap();
    symlink(&real_dir, h.workspace.join("linked")).unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let untracked = untracked_paths(&status);

    // The symlink itself should be listed, but its contents should not
    // (because we don't follow symlinks to directories).
    assert!(
        untracked.contains(&"linked".to_owned()),
        "symlinked directory should appear as untracked"
    );
    assert!(
        !untracked.iter().any(|p| p.starts_with("linked/")),
        "contents of symlinked directory should not be traversed"
    );
}

#[test]
fn builtin_wheat_directory_is_always_ignored() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    let status = h.json_ok(&h.workspace, &["status"]);
    let ignored = ignored_paths(&status);

    // .wheat is a builtin ignore — always present.
    assert!(
        ignored.contains(&".wheat".to_owned()),
        ".wheat should always be ignored"
    );
    // And never in the untracked list.
    let untracked = untracked_paths(&status);
    assert!(
        !untracked.contains(&".wheat".to_owned()),
        ".wheat should not be untracked"
    );
}

#[test]
fn discovery_from_subdirectory() {
    let h = Harness::new();
    h.init_from_git(&[("src/main.rs", "fn main() {}\n")]);

    // Create an untracked file.
    fs::write(h.workspace.join("untracked.txt"), "hi\n").unwrap();

    // Run status from a subdirectory.
    let sub = h.workspace.join("src");
    let status = h.json_ok(&sub, &["status"]);
    let untracked = untracked_paths(&status);

    assert!(
        untracked.contains(&"untracked.txt".to_owned()),
        "status from subdirectory should still discover workspace-root untracked files"
    );
}

#[test]
fn directory_only_pattern_matches_directories_not_files() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    // `build/` should only match directories named build, not a file named build.
    fs::write(h.workspace.join(".wheatignore"), "build/\n").unwrap();
    fs::write(h.workspace.join("build"), "not a directory\n").unwrap();
    let build_dir = h.workspace.join("build-dir");
    fs::create_dir_all(&build_dir).unwrap();
    fs::write(build_dir.join("output.o"), "object\n").unwrap();

    let status = h.json_ok(&h.workspace, &["status"]);
    let untracked = untracked_paths(&status);

    // The file named "build" should be untracked (not ignored by `build/`).
    assert!(
        untracked.contains(&"build".to_owned()),
        "file named 'build' should not be matched by 'build/' pattern"
    );
}

#[test]
fn schema_version_is_two_with_untracked() {
    let h = Harness::new();
    h.init_from_git(&[("tracked.txt", "hello\n")]);

    let status = h.json_ok(&h.workspace, &["status"]);
    assert_eq!(
        status["result"]["schema_version"], 2,
        "status schema version should be 2 with untracked discovery"
    );
}
