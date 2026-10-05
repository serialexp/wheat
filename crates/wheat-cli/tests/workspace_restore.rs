#![cfg(target_os = "linux")]

use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Restore Test")
        .env("GIT_AUTHOR_EMAIL", "restore@example.invalid")
        .env("GIT_COMMITTER_NAME", "Restore Test")
        .env("GIT_COMMITTER_EMAIL", "restore@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn adjusted_args<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    let mut args = args.to_vec();
    if args.windows(2).any(|pair| pair == ["change", "new"])
        && !args
            .iter()
            .any(|argument| *argument == "--message" || *argument == "-m")
    {
        args.extend(["--message", "test revision"]);
    }
    args
}

fn test_config(stores: &Path) -> std::path::PathBuf {
    let home = stores.join("test-config");
    fs::create_dir_all(home.join("wheat")).unwrap();
    fs::write(
        home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    home
}

fn wheat(stores: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(adjusted_args(args))
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", test_config(stores))
        .output()
        .unwrap()
}

fn wheat_failing_at(stores: &Path, args: &[&str], checkpoint: &str) -> Output {
    wheat_failing_at_occurrence(stores, args, checkpoint, 1)
}

fn wheat_failing_at_occurrence(
    stores: &Path,
    args: &[&str],
    checkpoint: &str,
    occurrence: usize,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(adjusted_args(args))
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", test_config(stores))
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
        .env(
            "WHEAT_TEST_MATERIALIZATION_FAIL_OCCURRENCE",
            occurrence.to_string(),
        )
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "exit")
        .output()
        .unwrap()
}

fn wheat_killed_at(stores: &Path, args: &[&str], checkpoint: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(adjusted_args(args))
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", test_config(stores))
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
        .output()
        .unwrap()
}

fn paused_wheat(stores: &Path, args: &[&str], checkpoint: &str, pause: &Path) -> Child {
    fs::write(pause, b"wait\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(adjusted_args(args))
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", test_config(stores))
        .env("WHEAT_TEST_MATERIALIZATION_PAUSE_AT", checkpoint)
        .env("WHEAT_TEST_MATERIALIZATION_PAUSE_FILE", pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn release_pause(child: Child, pause: &Path) -> Output {
    let reached = pause.with_extension("reached");
    for _ in 0..5_000 {
        if reached.exists() {
            fs::remove_file(pause).unwrap();
            return child.wait_with_output().unwrap();
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("restore did not reach synchronization checkpoint");
}

fn assert_no_materialization_transaction(workspace: &Path) {
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());
}

fn transaction_directory(workspace: &Path) -> std::path::PathBuf {
    let transactions = workspace.join(".wheat/materialization-transactions");
    fs::read_dir(transactions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    git(
        root.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/a"), b"a\n").unwrap();
    fs::write(source.join("dir/b"), b"b\n").unwrap();
    fs::write(source.join("other"), b"other\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "root"]);
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let output = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (root, workspace, stores)
}

#[test]
fn restores_one_file_from_large_unrelated_tree() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    git(
        root.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    fs::create_dir(source.join("selected")).unwrap();
    fs::write(source.join("selected/file"), b"selected\n").unwrap();
    for index in 0..1_000 {
        let directory = source.join(format!("unrelated-{index:04}/deep"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("leaf"), b"unrelated\n").unwrap();
    }
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "large tree"]);
    let workspace = root.path().join("workspace");
    let stores = root.path().join("stores");
    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );

    fs::write(workspace.join("selected/file"), b"dirty\n").unwrap();
    fs::write(workspace.join("unrelated-0999/deep/leaf"), b"keep dirty\n").unwrap();
    let restored = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "selected/file",
        ],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("selected/file")).unwrap(),
        b"selected\n"
    );
    assert_eq!(
        fs::read(workspace.join("unrelated-0999/deep/leaf")).unwrap(),
        b"keep dirty\n"
    );
    assert!(String::from_utf8_lossy(&restored.stdout).contains("restored 1 tracked path(s)"));
}

#[test]
fn restores_leaf_and_recursive_directory_with_overlap_without_touching_siblings() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"dirty a\n").unwrap();
    fs::write(workspace.join("dir/b"), b"dirty b\n").unwrap();
    fs::write(workspace.join("other"), b"keep dirty\n").unwrap();
    fs::write(workspace.join("dir/untracked"), b"mine\n").unwrap();
    let metadata = fs::read(workspace.join(".wheat/workspace.json")).unwrap();

    let output = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir",
            "dir/a",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"keep dirty\n");
    assert_eq!(
        fs::read(workspace.join("dir/untracked")).unwrap(),
        b"mine\n"
    );
    assert_eq!(
        fs::read(workspace.join(".wheat/workspace.json")).unwrap(),
        metadata
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("restored 2 tracked path(s)"));
}

#[test]
fn every_selector_is_validated_even_when_an_earlier_selection_is_clean() {
    let (_root, workspace, stores) = fixture();
    for invalid in ["../other", ".wheat/workspace.json", "dir/missing"] {
        let output = wheat(
            &stores,
            &[
                "--workspace",
                workspace.to_str().unwrap(),
                "restore",
                "dir/a",
                invalid,
            ],
        );
        assert!(!output.status.success(), "{invalid:?}");
        assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
        assert!(!workspace.join(".wheat/materialization.json").exists());
    }
}

#[test]
fn absent_pinned_selector_errors_and_preserves_workspace_only_content() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("workspace-only"), b"mine\n").unwrap();
    let output = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "workspace-only",
        ],
    );
    assert!(!output.status.success());
    assert_eq!(
        fs::read(workspace.join("workspace-only")).unwrap(),
        b"mine\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn repository_authority_advancement_does_not_retarget_restore() {
    let (_root, workspace, stores) = fixture();
    let advanced = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "change",
            "new",
            "--path",
            "dir/a",
            "--expected",
            "a\n",
            "--value",
            "authority value\n",
        ],
    );
    assert!(
        advanced.status.success(),
        "{}",
        String::from_utf8_lossy(&advanced.stderr)
    );
    fs::write(workspace.join("dir/a"), b"workspace value\n").unwrap();

    let restored = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
}

#[test]
fn interrupted_restore_recovers_exact_observed_preimage_without_retargeting() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"dirty a\n").unwrap();
    let metadata = fs::read(workspace.join(".wheat/workspace.json")).unwrap();
    let failed = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
        "exchanged",
    );
    assert!(!failed.status.success());

    let recovered = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(
        fs::read(workspace.join(".wheat/workspace.json")).unwrap(),
        metadata
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[cfg(unix)]
#[test]
fn clean_restore_is_a_true_noop_without_journal_or_metadata_change() {
    let (_root, workspace, stores) = fixture();
    let path = workspace.join("dir/a");
    let before_file = fs::metadata(&path).unwrap();
    let before_metadata = fs::read(workspace.join(".wheat/workspace.json")).unwrap();

    let output = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after_file = fs::metadata(&path).unwrap();
    assert_eq!(before_file.ino(), after_file.ino());
    assert_eq!(before_file.mtime(), after_file.mtime());
    assert_eq!(before_file.mtime_nsec(), after_file.mtime_nsec());
    assert_eq!(
        before_metadata,
        fs::read(workspace.join(".wheat/workspace.json")).unwrap()
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(
        !transactions.exists() || fs::read_dir(transactions).unwrap().next().is_none(),
        "no-op restore created a transaction"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("restored 0 tracked path(s)"));
}

#[test]
fn completion_and_cleanup_interruptions_never_resurrect_discarded_bytes() {
    for checkpoint in [
        "completion-persisted",
        "staged-preimage-unlinked",
        "journal-unlinked",
    ] {
        let (_root, workspace, stores) = fixture();
        fs::write(workspace.join("dir/a"), b"discard me\n").unwrap();
        let interrupted = wheat_killed_at(
            &stores,
            &[
                "--workspace",
                workspace.to_str().unwrap(),
                "restore",
                "dir/a",
            ],
            checkpoint,
        );
        assert!(!interrupted.status.success(), "{checkpoint}");
        let recovered = wheat(
            &stores,
            &["--workspace", workspace.to_str().unwrap(), "status"],
        );
        assert!(
            recovered.status.success(),
            "{checkpoint}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
        assert!(!workspace.join(".wheat/materialization.json").exists());
    }
}

#[test]
fn edit_after_restore_intent_aborts_replacement_and_retains_completed_prefix() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    fs::write(workspace.join("dir/b"), b"discard b\n").unwrap();
    let interrupted = wheat_failing_at_occurrence(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
            "dir/b",
        ],
        // Both intents are recorded in one batch; stop after the first is applied.
        "installed",
        1,
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir/b"), b"concurrent b\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"));
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(
        fs::read(workspace.join("dir/b")).unwrap(),
        b"concurrent b\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/b",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
}

#[test]
fn directory_after_regular_replacement_intent_aborts_and_allows_structural_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("other"), b"discard other\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "intent-persisted",
    );
    assert!(!interrupted.status.success());
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(workspace.join("other").is_dir());
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
}

#[test]
fn directory_after_absent_creation_intent_aborts_and_allows_structural_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("other")).unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "intent-persisted",
    );
    assert!(!interrupted.status.success());
    fs::create_dir(workspace.join("other")).unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(workspace.join("other").is_dir());
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
}

#[test]
fn edit_after_restore_intent_aborts_empty_directory_replacement_and_allows_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "intent-persisted",
    );
    assert!(!interrupted.status.success());
    fs::remove_dir(workspace.join("other")).unwrap();
    fs::write(workspace.join("other"), b"concurrent other\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"concurrent other\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
}

#[test]
fn edit_after_replacement_staging_aborts_and_consumes_unintended_artifact() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("other"), b"discard other\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "staged",
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("other"), b"concurrent other\n").unwrap();

    let abort_interrupted = wheat_failing_at(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        "abort-persisted",
    );
    assert!(!abort_interrupted.status.success());
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"concurrent other\n"
    );
    assert!(workspace.join(".wheat/materialization.json").exists());

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"concurrent other\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
}

#[test]
fn abort_cleanup_refuses_tampered_unintended_staged_artifact() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("other"), b"discard other\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "staged",
    );
    assert!(!interrupted.status.success());
    let transaction = transaction_directory(&workspace);
    fs::write(
        transaction.join("leaf-00000000"),
        b"tampered transaction data\n",
    )
    .unwrap();
    fs::write(workspace.join("other"), b"concurrent other\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr)
            .contains("refusing to remove unauthenticated aborted restore debris")
    );
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"concurrent other\n"
    );
    assert_eq!(
        fs::read(transaction.join("leaf-00000000")).unwrap(),
        b"tampered transaction data\n"
    );
    assert!(workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn abort_cleanup_does_not_treat_unprepared_later_operation_as_debris() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    fs::write(workspace.join("dir/b"), b"discard b\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
            "dir/b",
        ],
        "staged",
    );
    assert!(!interrupted.status.success());
    let transaction = transaction_directory(&workspace);
    fs::write(transaction.join("leaf-00000001"), b"unowned later data\n").unwrap();
    fs::write(workspace.join("dir/a"), b"concurrent a\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert_eq!(
        fs::read(workspace.join("dir/a")).unwrap(),
        b"concurrent a\n"
    );
    assert_eq!(
        fs::read(transaction.join("leaf-00000001")).unwrap(),
        b"unowned later data\n"
    );
    assert!(workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn edit_after_empty_directory_staging_aborts_and_consumes_unintended_artifact() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "staged",
    );
    assert!(!interrupted.status.success());
    fs::remove_dir(workspace.join("other")).unwrap();
    fs::write(workspace.join("other"), b"concurrent other\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"concurrent other\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
}

#[test]
fn edit_after_structural_tombstone_staging_aborts_and_consumes_unintended_artifact() {
    let (_root, workspace, stores) = fixture();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"original obstruction\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
        "staged",
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir"), b"concurrent obstruction\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert_eq!(
        fs::read(workspace.join("dir")).unwrap(),
        b"concurrent obstruction\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
}

#[test]
fn edit_after_journal_publication_aborts_recovery_and_allows_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
        "journal-created",
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir/a"), b"new edit\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"));
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"new edit\n");
    assert!(!workspace.join(".wheat/materialization.json").exists());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
}

#[test]
fn changed_remaining_path_aborts_recovery_without_wedge_and_rerun_uses_fresh_consent() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    fs::write(workspace.join("dir/b"), b"discard b\n").unwrap();
    let interrupted = wheat_failing_at_occurrence(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
            "dir/b",
        ],
        "installed",
        1,
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir/b"), b"new edit\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"));
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"new edit\n");
    assert!(!workspace.join(".wheat/materialization.json").exists());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/b",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
}

#[test]
fn concurrent_structural_change_before_recheck_aborts_without_wedging_restore() {
    let (root, workspace, stores) = fixture();
    fs::write(workspace.join("other"), b"discard other\n").unwrap();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"observed obstruction\n").unwrap();
    let pause = root.path().join("restore-structural-recheck.pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir",
            "other",
        ],
        "before-structural-recheck",
        &pause,
    );
    let reached = pause.with_extension("reached");
    for _ in 0..5_000 {
        if reached.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(reached.exists());
    fs::remove_file(workspace.join("dir")).unwrap();
    fs::create_dir(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir/concurrent"), b"concurrent data\n").unwrap();

    let invalidated = release_pause(child, &pause);
    assert!(!invalidated.status.success());
    assert!(
        String::from_utf8_lossy(&invalidated.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&invalidated.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("dir/concurrent")).unwrap(),
        b"concurrent data\n"
    );
    assert_eq!(
        fs::read(workspace.join("other")).unwrap(),
        b"discard other\n"
    );
    assert_no_materialization_transaction(&workspace);

    let rerun = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
    assert_eq!(
        fs::read(workspace.join("dir/concurrent")).unwrap(),
        b"concurrent data\n"
    );
}

#[test]
fn concurrent_insertion_before_empty_directory_removal_preserves_completed_prefix() {
    let (root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    let pause = root.path().join("restore-boundary-removal.pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
            "other",
        ],
        "before-empty-directory-removal",
        &pause,
    );
    let reached = pause.with_extension("reached");
    for _ in 0..5_000 {
        if reached.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(reached.exists());
    fs::write(workspace.join("other/concurrent"), b"concurrent data\n").unwrap();

    let invalidated = release_pause(child, &pause);
    assert!(!invalidated.status.success());
    assert!(
        String::from_utf8_lossy(&invalidated.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&invalidated.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(
        fs::read(workspace.join("other/concurrent")).unwrap(),
        b"concurrent data\n"
    );
    assert_no_materialization_transaction(&workspace);

    let refused = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(
        fs::read(workspace.join("other/concurrent")).unwrap(),
        b"concurrent data\n"
    );
    assert_no_materialization_transaction(&workspace);

    fs::remove_file(workspace.join("other/concurrent")).unwrap();
    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
}

#[test]
fn pinned_leaf_replaces_only_an_observed_empty_directory() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    let restored = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
    assert!(String::from_utf8_lossy(&restored.stdout).contains("restored 1 tracked path(s)"));

    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    fs::write(workspace.join("other/mine"), b"mine\n").unwrap();
    let refused = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(fs::read(workspace.join("other/mine")).unwrap(), b"mine\n");
}

#[test]
fn pinned_directory_selector_replaces_workspace_leaf_but_descendant_selector_refuses_it() {
    let (_root, workspace, stores) = fixture();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"obstruction\n").unwrap();

    let refused = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(fs::read(workspace.join("dir")).unwrap(), b"obstruction\n");

    let restored = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
    assert!(String::from_utf8_lossy(&restored.stdout).contains("restored 2 tracked path(s)"));
}

#[test]
fn edit_after_structural_deletion_intent_preserves_obstruction_and_allows_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"original obstruction\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
        "intent-persisted",
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir"), b"concurrent obstruction\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("dir")).unwrap(),
        b"concurrent obstruction\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
}

#[test]
fn directory_after_structural_obstruction_intent_aborts_without_wedging_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"original obstruction\n").unwrap();
    let interrupted = wheat_failing_at(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
        "intent-persisted",
    );
    assert!(!interrupted.status.success());
    fs::remove_file(workspace.join("dir")).unwrap();
    fs::create_dir(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir/concurrent"), b"concurrent data\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("dir/concurrent")).unwrap(),
        b"concurrent data\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(fs::read_dir(transactions).unwrap().next().is_none());

    let rerun = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "restore", "dir"],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(
        fs::read(workspace.join("dir/concurrent")).unwrap(),
        b"concurrent data\n"
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn structural_obstruction_abort_cleanup_preserves_changed_leaf_and_allows_rerun() {
    let (_root, workspace, stores) = fixture();
    fs::remove_dir_all(workspace.join("dir")).unwrap();
    fs::write(workspace.join("dir"), b"obstruction\n").unwrap();
    fs::write(workspace.join("other"), b"discard other\n").unwrap();

    let interrupted = wheat_failing_at_occurrence(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir",
            "other",
        ],
        "installed",
        1,
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("other"), b"new edit\n").unwrap();

    let recovery = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(!recovery.status.success());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("preserved the new value"));
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"b\n");
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"new edit\n");
    assert!(!workspace.join(".wheat/materialization.json").exists());

    let rerun = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
    );
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
    assert!(String::from_utf8_lossy(&rerun.stdout).contains("restored 1 tracked path(s)"));
}

#[test]
fn aborted_journal_unlink_crash_is_cleaned_as_terminal_orphan() {
    let (_root, workspace, stores) = fixture();
    fs::write(workspace.join("dir/a"), b"discard a\n").unwrap();
    fs::write(workspace.join("dir/b"), b"discard b\n").unwrap();
    let interrupted = wheat_failing_at_occurrence(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
            "dir/b",
        ],
        "installed",
        1,
    );
    assert!(!interrupted.status.success());
    fs::write(workspace.join("dir/b"), b"new edit\n").unwrap();
    let aborted = wheat_killed_at(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        "aborted-journal-unlinked",
    );
    assert!(!aborted.status.success());

    let recovered = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
    assert_eq!(fs::read(workspace.join("dir/b")).unwrap(), b"new edit\n");
}

#[test]
fn empty_directory_removal_after_intent_recovers() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("other")).unwrap();
    fs::create_dir(workspace.join("other")).unwrap();
    let interrupted = wheat_killed_at(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "other",
        ],
        "empty-directory-removed",
    );
    assert!(!interrupted.status.success());

    let recovered = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(fs::read(workspace.join("other")).unwrap(), b"other\n");
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn restore_recreates_a_deleted_tracked_leaf() {
    let (_root, workspace, stores) = fixture();
    fs::remove_file(workspace.join("dir/a")).unwrap();
    let output = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "restore",
            "dir/a",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(workspace.join("dir/a")).unwrap(), b"a\n");
}
