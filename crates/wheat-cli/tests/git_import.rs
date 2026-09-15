#![cfg(target_os = "linux")]

use std::{path::Path, process::Command};

fn git(directory: Option<&Path>, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Wheat Test")
        .env("GIT_AUTHOR_EMAIL", "wheat@example.invalid")
        .env("GIT_COMMITTER_NAME", "Wheat Test")
        .env("GIT_COMMITTER_EMAIL", "wheat@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn wheat(store_home: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(arguments)
        .env("WHEAT_STORE_HOME", store_home)
        .output()
        .unwrap()
}

fn trace_records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn fixture(root: &Path) -> std::path::PathBuf {
    let source = root.join("source");
    git(None, &["init", "-b", "main", source.to_str().unwrap()]);
    std::fs::write(source.join("main.txt"), b"main\r\n").unwrap();
    git(Some(&source), &["add", "main.txt"]);
    git(Some(&source), &["commit", "-m", "main"]);
    git(Some(&source), &["checkout", "-b", "feature"]);
    std::fs::write(source.join("feature.txt"), b"feature\n").unwrap();
    git(Some(&source), &["add", "feature.txt"]);
    git(Some(&source), &["commit", "-m", "feature"]);
    source
}

#[test]
fn imports_and_materializes_a_selected_git_branch_in_a_separate_store() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");

    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/feature",
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let stdout = String::from_utf8_lossy(&imported.stdout);
    assert!(stdout.contains("imported refs/heads/feature at generation 0"));
    assert_eq!(
        std::fs::read(workspace.join("main.txt")).unwrap(),
        b"main\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("feature.txt")).unwrap(),
        b"feature\n"
    );
    assert!(workspace.join(".wheat/workspace.json").is_file());
    assert!(!workspace.join("repository.head").exists());
    let stores = std::fs::read_dir(&stores)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let store = stores
        .iter()
        .find(|entry| entry.file_type().unwrap().is_dir())
        .expect("one canonical store directory");
    assert_eq!(
        stores
            .iter()
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .count(),
        1
    );
    assert!(store.path().join("repository.head").is_file());
    assert!(store.path().join("storage").is_dir());

    let nested = workspace.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let status = wheat(
        &root.path().join("stores"),
        &["--workspace", nested.to_str().unwrap(), "status"],
    );
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(String::from_utf8_lossy(&status.stdout).contains("tracked changes: 0"));

    std::fs::write(workspace.join("main.txt"), b"main\r\n").unwrap();
    let canonical_clean = wheat(
        &root.path().join("stores"),
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(canonical_clean.status.success());
    assert!(String::from_utf8_lossy(&canonical_clean.stdout).contains("tracked changes: 0"));

    std::fs::write(workspace.join("main.txt"), b"changed\n").unwrap();
    let dirty = wheat(
        &root.path().join("stores"),
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "status",
            "--format",
            "json",
        ],
    );
    assert!(dirty.status.success());
    let json: serde_json::Value = serde_json::from_slice(&dirty.stdout).unwrap();
    assert_eq!(json["result"]["changes"][0]["path"], "main.txt");
    assert_eq!(json["result"]["changes"][0]["content_changed"], true);
}

#[test]
fn import_trace_is_bounded_jsonl_and_keeps_command_output_separate() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let trace = root.path().join("import-trace.jsonl");

    let imported = wheat(
        &stores,
        &[
            "--format=json",
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/feature",
            "--trace",
            trace.to_str().unwrap(),
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(stdout["command"], "import");

    let records = trace_records(&trace);
    assert!(records.len() < 80, "small imports emit only coarse events");
    assert_eq!(records.first().unwrap()["event"], "import_started");
    assert_eq!(records.last().unwrap()["event"], "import_complete");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["schema"], "wheat.import-trace.v1");
        assert_eq!(record["seq"], index as u64);
        if index > 0 {
            assert!(
                record["elapsed_us"].as_u64().unwrap()
                    >= records[index - 1]["elapsed_us"].as_u64().unwrap()
            );
        }
    }
    let progress = records
        .iter()
        .find(|record| record["event"] == "stage_progress")
        .unwrap();
    assert_eq!(progress["data"]["complete"], true);
    assert_eq!(progress["data"]["completed_commits"], 2);
    assert!(
        progress["data"]["storage"]["deferred_put_attempts"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "validation_phase_started")
            .map(|record| record["data"]["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "load_state",
            "index_retained",
            "verify_content",
            "verify_changes",
            "verify_lineage",
            "verify_view_content_sets",
            "verify_view_trees",
            "verify_view_revision_maps",
            "verify_view_application_maps",
            "verify_revisions",
            "verify_applications",
        ]
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "finalize_phase_started")
            .map(|record| record["data"]["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "preflight",
            "journal_auth_accumulation",
            "aggregate_set_construction",
            "batch_flush",
            "prepare_envelope_validation",
            "head_initialize",
        ]
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record["event"] == "phase_started" && record["data"]["phase"] == "resolve_frontier"
            })
            .count(),
        1
    );
    let trace_text = std::fs::read_to_string(&trace).unwrap();
    assert!(!trace_text.contains(source.to_str().unwrap()));
    assert!(!trace_text.contains(workspace.to_str().unwrap()));
}

#[test]
fn import_trace_must_be_a_fresh_file_before_import_changes_state() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let trace = root.path().join("existing.jsonl");
    std::fs::write(&trace, "preserve me\n").unwrap();

    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--trace",
            trace.to_str().unwrap(),
        ],
    );
    assert!(!imported.status.success());
    assert_eq!(std::fs::read_to_string(trace).unwrap(), "preserve me\n");
    assert!(!workspace.exists());
    assert_eq!(std::fs::read_dir(stores).unwrap().count(), 0);
}

#[test]
fn import_help_documents_trace_path() {
    let root = tempfile::tempdir().unwrap();
    let help = wheat(root.path(), &["import", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--trace <PATH>"));
}

#[test]
fn imports_a_remote_git_url_through_a_temporary_mirror() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let remote = root.path().join("remote.git");
    git(
        None,
        &[
            "clone",
            "--mirror",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let remote_url = format!("file://{}", remote.display());

    let imported = wheat(
        &stores,
        &[
            "import",
            &remote_url,
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/feature",
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert!(workspace.join("feature.txt").is_file());
}

#[test]
fn import_preserves_nonempty_workspace_and_reports_unknown_branch() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("sentinel"), b"keep").unwrap();

    let existing = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!existing.status.success());
    assert!(String::from_utf8_lossy(&existing.stderr).contains("not empty"));
    assert_eq!(std::fs::read(workspace.join("sentinel")).unwrap(), b"keep");

    let missing_workspace = root.path().join("missing-branch");
    let missing = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            missing_workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/missing",
        ],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("working branch \"refs/heads/missing\" is not in the pinned Git snapshot")
    );
    assert!(!missing_workspace.exists());
}

#[test]
fn remote_clone_failure_does_not_create_workspace_or_store() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let missing_remote = format!("file://{}", root.path().join("missing.git").display());

    let failed = wheat(
        &stores,
        &["import", &missing_remote, workspace.to_str().unwrap()],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("git clone for Wheat import failed"));
    assert!(!workspace.exists());
    assert!(std::fs::read_dir(stores).unwrap().next().is_none());
}
