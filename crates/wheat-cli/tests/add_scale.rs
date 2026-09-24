//! `add` over thousands of files in one directory.
//!
//! One revision once could not hold more than 4,096 edits, and every per-path tree lookup
//! decoded the shared directory again, so `add`, and `validate` after it, grew with the
//! square of the file count. These tests pin both: the command succeeds past the old
//! limit, and each step stays inside a budget a quadratic step cannot meet.
//!
//! The larger benchmark is ignored by default:
//! `cargo nextest run -p wheat-cli --test add_scale --run-ignored only --no-capture`
//! (`WHEAT_ADD_SCALE_FILES` overrides its file count).
#![cfg(target_os = "linux")]

use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

use serde_json::Value;

/// More than the former 4,096-edit revision limit.
const FILES: usize = 5_000;
/// Default size of the ignored benchmark.
const BENCHMARK_FILES: usize = 20_000;
/// Budget for one command in a debug build. At 5,000 files the linear steps take about
/// two seconds; the quadratic ones they replaced took 65 s (`add` of new files) and
/// 200 s (`add` of modified files). At 20,000 files `validate` took 127 s.
const STEP_BUDGET: Duration = Duration::from_secs(30);

struct Workspace {
    _directory: tempfile::TempDir,
    root: PathBuf,
    stores: PathBuf,
    config: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspace");
        let stores = directory.path().join("stores");
        let config = directory.path().join("config");
        std::fs::create_dir_all(config.join("wheat")).unwrap();
        std::fs::write(
            config.join("wheat/config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();
        let workspace = Self {
            _directory: directory,
            root,
            stores,
            config,
        };
        workspace.ok(&["init", workspace.root.to_str().unwrap()]);
        workspace
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .arg("--workspace")
            .arg(&self.root)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config)
            .output()
            .expect("wheat command runs")
    }

    fn ok(&self, arguments: &[&str]) -> Output {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "wheat {arguments:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    /// Runs one command and fails if it exceeds [`STEP_BUDGET`].
    fn timed(&self, step: &str, arguments: &[&str]) -> Output {
        let started = Instant::now();
        let output = self.ok(arguments);
        let elapsed = started.elapsed();
        eprintln!("{step}: {:.2} s", elapsed.as_secs_f64());
        assert!(
            elapsed <= STEP_BUDGET,
            "{step} took {elapsed:?}, over the {STEP_BUDGET:?} budget"
        );
        output
    }

    /// Writes `files` files into one directory and returns their workspace paths.
    fn write_files(&self, files: usize, version: &str) -> Vec<String> {
        let directory = self.root.join("src");
        std::fs::create_dir_all(&directory).unwrap();
        (0..files)
            .map(|index| {
                let path = format!("src/f{index:06}.txt");
                std::fs::write(
                    self.root.join(&path),
                    format!("file {index}\nversion {version}\n"),
                )
                .unwrap();
                path
            })
            .collect()
    }

    fn add(&self, step: &str, paths: &[String]) {
        let mut arguments = vec!["add", "--message", step];
        arguments.extend(paths.iter().map(String::as_str));
        self.timed(step, &arguments);
    }

    /// Asserts the workspace holds exactly the working view and nothing is left to add.
    fn assert_settled(&self) {
        let output = self.timed("status", &["--format=json", "status"]);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        let result = &status["result"];
        assert_eq!(result["changes"], Value::Array(Vec::new()), "{status}");
        assert_eq!(
            result["materialized_view"], result["effective_view"],
            "add left materialization pending"
        );
    }
}

fn add_new_then_modified(files: usize) {
    let workspace = Workspace::new();
    workspace.ok(&["change", "new", "--message", "create"]);
    let paths = workspace.write_files(files, "one");
    workspace.add("add new files", &paths);
    workspace.assert_settled();

    workspace.ok(&["change", "new", "--message", "modify"]);
    workspace.write_files(files, "two");
    workspace.add("add modified files", &paths);
    workspace.assert_settled();

    workspace.timed("validate", &["validate"]);
}

#[test]
fn add_past_the_former_revision_edit_limit() {
    add_new_then_modified(FILES);
}

#[test]
#[ignore = "benchmark: run explicitly"]
fn add_scale_benchmark() {
    let files = std::env::var("WHEAT_ADD_SCALE_FILES")
        .map(|value| value.parse().expect("WHEAT_ADD_SCALE_FILES is a number"))
        .unwrap_or(BENCHMARK_FILES);
    add_new_then_modified(files);
}
