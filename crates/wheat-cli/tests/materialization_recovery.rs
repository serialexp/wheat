use std::{
    path::Path,
    process::{Child, Command, Output},
    thread,
    time::Duration,
};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Recovery Test")
        .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
        .env("GIT_COMMITTER_NAME", "Recovery Test")
        .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wheat(stores: &Path, args: &[&str], checkpoint: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
    command.args(args).env("WHEAT_STORE_HOME", stores);
    if let Some(checkpoint) = checkpoint {
        command
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "exit");
    }
    command.output().unwrap()
}

fn paused_wheat(
    stores: &Path,
    args: &[&str],
    checkpoint: &str,
    occurrence: usize,
    pause: &Path,
) -> Child {
    std::fs::write(pause, b"wait\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(args)
        .env("WHEAT_STORE_HOME", stores)
        .env("WHEAT_TEST_MATERIALIZATION_PAUSE_AT", checkpoint)
        .env(
            "WHEAT_TEST_MATERIALIZATION_FAIL_OCCURRENCE",
            occurrence.to_string(),
        )
        .env("WHEAT_TEST_MATERIALIZATION_PAUSE_FILE", pause)
        .spawn()
        .unwrap()
}

fn release_pause(child: Child, pause: &Path) -> Output {
    let reached = pause.with_extension("reached");
    for _ in 0..5_000 {
        if reached.exists() {
            std::fs::remove_file(pause).unwrap();
            return child.wait_with_output().unwrap();
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("materialization did not reach synchronization checkpoint");
}

fn source(root: &Path) -> std::path::PathBuf {
    let source = root.join("source");
    git(root, &["init", "-b", "main", source.to_str().unwrap()]);
    std::fs::create_dir(source.join("nested")).unwrap();
    std::fs::write(source.join("a"), b"a\n").unwrap();
    std::fs::write(source.join("nested/b"), b"b\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "root"]);
    source
}

fn source_with_files(root: &Path, count: usize) -> std::path::PathBuf {
    let source = root.join("source");
    git(root, &["init", "-b", "main", source.to_str().unwrap()]);
    for index in 0..count {
        std::fs::write(source.join(format!("file-{index:05}")), b"content\n").unwrap();
    }
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "root"]);
    source
}

fn source_with_delta(root: &Path) -> std::path::PathBuf {
    let source = root.join("source");
    git(root, &["init", "-b", "main", source.to_str().unwrap()]);
    std::fs::write(source.join("same"), b"same\n").unwrap();
    std::fs::write(source.join("replace"), b"old\n").unwrap();
    std::fs::write(source.join("delete"), b"delete\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "main"]);
    git(&source, &["checkout", "-b", "feature"]);
    std::fs::write(source.join("replace"), b"new\n").unwrap();
    std::fs::remove_file(source.join("delete")).unwrap();
    std::fs::write(source.join("create"), b"create\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-m", "feature"]);
    source
}

#[test]
fn interrupted_initial_materialization_rolls_forward_without_overwriting() {
    for checkpoint in [
        "journal-created",
        "staged",
        "intent-persisted",
        "installed",
        "all-installed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let stores = root.path().join("stores");
        let source = source(root.path());
        let workspace = root.path().join("workspace");
        let failed = wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
            ],
            Some(checkpoint),
        );
        assert!(
            !failed.status.success(),
            "checkpoint {checkpoint} unexpectedly succeeded"
        );
        assert!(workspace.join(".wheat/materialization.json").is_file());

        let recovered = wheat(
            &stores,
            &["--workspace", workspace.to_str().unwrap(), "status"],
            None,
        );
        assert!(
            recovered.status.success(),
            "checkpoint {checkpoint}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(std::fs::read(workspace.join("a")).unwrap(), b"a\n");
        assert_eq!(std::fs::read(workspace.join("nested/b")).unwrap(), b"b\n");
        assert!(!workspace.join(".wheat/materialization.json").exists());
    }
}

#[test]
fn progress_storage_scales_linearly_with_file_count() {
    const FILES: usize = 1_000;
    const RECORD_BYTES: u64 = 36;
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_files(root.path(), FILES);
    let workspace = root.path().join("workspace");
    let failed = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
        Some("all-installed"),
    );
    assert!(!failed.status.success());

    let transactions = workspace.join(".wheat/materialization-transactions");
    let transaction = std::fs::read_dir(transactions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::metadata(transaction.join("progress.log"))
            .unwrap()
            .len(),
        u64::try_from(FILES).unwrap() * RECORD_BYTES
    );
    let plan_bytes = std::fs::metadata(workspace.join(".wheat/materialization.json"))
        .unwrap()
        .len();
    assert!(
        plan_bytes < u64::try_from(FILES).unwrap() * 700,
        "unexpectedly large immutable plan: {plan_bytes} bytes"
    );
}

#[test]
fn rematerializes_only_the_delta_and_round_trips_a_to_b_to_a() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
        None,
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let same_metadata = std::fs::metadata(workspace.join("same")).unwrap();

    let to_feature = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        None,
    );
    assert!(
        to_feature.status.success(),
        "{}",
        String::from_utf8_lossy(&to_feature.stderr)
    );
    assert_eq!(std::fs::read(workspace.join("replace")).unwrap(), b"new\n");
    assert!(!workspace.join("delete").exists());
    assert_eq!(
        std::fs::read(workspace.join("create")).unwrap(),
        b"create\n"
    );
    assert_eq!(
        same_metadata.ino(),
        std::fs::metadata(workspace.join("same")).unwrap().ino()
    );

    let to_main = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/main",
        ],
        None,
    );
    assert!(
        to_main.status.success(),
        "{}",
        String::from_utf8_lossy(&to_main.stderr)
    );
    assert_eq!(std::fs::read(workspace.join("replace")).unwrap(), b"old\n");
    assert_eq!(
        std::fs::read(workspace.join("delete")).unwrap(),
        b"delete\n"
    );
    assert!(!workspace.join("create").exists());
}

#[test]
fn rematerializes_file_directory_transitions_and_preserves_untracked_directories() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("source");
    git(
        root.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    std::fs::write(source.join("file-to-dir"), b"leaf\n").unwrap();
    std::fs::create_dir(source.join("dir-to-file")).unwrap();
    std::fs::write(source.join("dir-to-file/child"), b"child\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "main"]);
    let workspace = root.path().join("workspace");
    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
        None,
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    for args in [
        vec![
            "change",
            "new",
            "--path",
            "file-to-dir",
            "--expected",
            "leaf\n",
            "--delete",
        ],
        vec![
            "change",
            "new",
            "--path",
            "dir-to-file/child",
            "--expected",
            "child\n",
            "--delete",
        ],
        vec![
            "change",
            "new",
            "--path",
            "file-to-dir/child",
            "--value",
            "new child\n",
        ],
        vec![
            "change",
            "new",
            "--path",
            "dir-to-file",
            "--value",
            "new leaf\n",
        ],
    ] {
        let changed = wheat(
            &stores,
            &[
                &["--workspace", workspace.to_str().unwrap()],
                args.as_slice(),
            ]
            .concat(),
            None,
        );
        assert!(
            changed.status.success(),
            "{}",
            String::from_utf8_lossy(&changed.stderr)
        );
    }
    let switched = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "working",
        ],
        None,
    );
    assert!(
        switched.status.success(),
        "{}",
        String::from_utf8_lossy(&switched.stderr)
    );
    assert_eq!(
        std::fs::read(workspace.join("file-to-dir/child")).unwrap(),
        b"new child\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("dir-to-file")).unwrap(),
        b"new leaf\n"
    );

    // A nonempty user-owned directory at the directory-to-file boundary is
    // never removed, even though all tracked children were authenticated.
    std::fs::remove_file(workspace.join("dir-to-file")).unwrap();
    std::fs::create_dir(workspace.join("dir-to-file")).unwrap();
    std::fs::write(workspace.join("dir-to-file/untracked"), b"mine\n").unwrap();
    let blocked = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/main",
        ],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(workspace.join("dir-to-file/untracked")).unwrap(),
        b"mine\n"
    );
}

#[test]
fn directory_to_file_rejects_untracked_sibling_without_mutation_or_journal() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("source-directory-preflight");
    git(
        root.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    std::fs::create_dir(source.join("node")).unwrap();
    std::fs::write(source.join("node/tracked"), b"tracked\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "directory"]);
    let workspace = root.path().join("workspace-directory-preflight");
    let imported = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
        None,
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    for args in [
        [
            "change",
            "new",
            "--path",
            "node/tracked",
            "--expected",
            "tracked\n",
            "--delete",
        ],
        [
            "change",
            "new",
            "--path",
            "node",
            "--value",
            "replacement\n",
            "",
        ],
    ] {
        let args = args
            .into_iter()
            .filter(|arg| !arg.is_empty())
            .collect::<Vec<_>>();
        let changed = wheat(
            &stores,
            &[
                &["--workspace", workspace.to_str().unwrap()],
                args.as_slice(),
            ]
            .concat(),
            None,
        );
        assert!(
            changed.status.success(),
            "{}",
            String::from_utf8_lossy(&changed.stderr)
        );
    }
    std::fs::write(workspace.join("node/untracked"), b"mine\n").unwrap();

    let blocked = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "working",
        ],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(workspace.join("node/tracked")).unwrap(),
        b"tracked\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("node/untracked")).unwrap(),
        b"mine\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
    let transactions = workspace.join(".wheat/materialization-transactions");
    assert!(std::fs::read_dir(transactions).unwrap().next().is_none());
}

#[test]
fn directory_to_file_mutation_recheck_preserves_tracked_files() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("source-directory-race");
    git(
        root.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    std::fs::create_dir(source.join("node")).unwrap();
    std::fs::write(source.join("node/tracked"), b"tracked\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "directory"]);
    let workspace = root.path().join("workspace-directory-race");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    for args in [
        vec![
            "change",
            "new",
            "--path",
            "node/tracked",
            "--expected",
            "tracked\n",
            "--delete",
        ],
        vec![
            "change",
            "new",
            "--path",
            "node",
            "--value",
            "replacement\n",
        ],
    ] {
        assert!(
            wheat(
                &stores,
                &[
                    &["--workspace", workspace.to_str().unwrap()],
                    args.as_slice()
                ]
                .concat(),
                None
            )
            .status
            .success()
        );
    }
    let pause = root.path().join("structural.pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "working",
        ],
        "before-structural-recheck",
        1,
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
    std::fs::write(workspace.join("node/untracked"), b"mine\n").unwrap();
    let output = release_pause(child, &pause);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(workspace.join("node/tracked")).unwrap(),
        b"tracked\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("node/untracked")).unwrap(),
        b"mine\n"
    );
}

#[test]
fn interrupted_directory_to_file_transition_recovers_structurally() {
    for checkpoint in [
        "intent-persisted",
        "exchanged",
        "deletion-unlinked",
        "installed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let stores = root.path().join("stores");
        let source = root.path().join("source-directory-crash");
        git(
            root.path(),
            &["init", "-b", "main", source.to_str().unwrap()],
        );
        std::fs::create_dir(source.join("node")).unwrap();
        std::fs::write(source.join("node/tracked"), b"tracked\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "directory"]);
        let workspace = root.path().join("workspace-directory-crash");
        assert!(
            wheat(
                &stores,
                &[
                    "import",
                    source.to_str().unwrap(),
                    workspace.to_str().unwrap(),
                    "--branch",
                    "refs/heads/main"
                ],
                None
            )
            .status
            .success()
        );
        for args in [
            vec![
                "change",
                "new",
                "--path",
                "node/tracked",
                "--expected",
                "tracked\n",
                "--delete",
            ],
            vec![
                "change",
                "new",
                "--path",
                "node",
                "--value",
                "replacement\n",
            ],
        ] {
            assert!(
                wheat(
                    &stores,
                    &[
                        &["--workspace", workspace.to_str().unwrap()],
                        args.as_slice()
                    ]
                    .concat(),
                    None
                )
                .status
                .success()
            );
        }
        let interrupted = Command::new(env!("CARGO_BIN_EXE_wheat"))
            .args([
                "--workspace",
                workspace.to_str().unwrap(),
                "materialize",
                "--source",
                "working",
            ])
            .env("WHEAT_STORE_HOME", &stores)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
            .output()
            .unwrap();
        assert!(!interrupted.status.success(), "{checkpoint}");
        let recovered = wheat(
            &stores,
            &["--workspace", workspace.to_str().unwrap(), "status"],
            None,
        );
        assert!(
            recovered.status.success(),
            "{checkpoint}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            std::fs::read(workspace.join("node")).unwrap(),
            b"replacement\n"
        );
        assert!(!workspace.join(".wheat/materialization.json").exists());
    }
}

#[test]
fn rematerialization_refuses_the_whole_plan_when_any_delta_path_is_dirty() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    std::fs::write(workspace.join("replace"), b"dirty\n").unwrap();
    let blocked = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"dirty\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("delete")).unwrap(),
        b"delete\n"
    );
    assert!(!workspace.join("create").exists());
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[test]
fn recovery_truncates_corrupt_final_full_width_progress_record() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace-corrupt-progress");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let interrupted = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ])
        .env("WHEAT_STORE_HOME", &stores)
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", "intent-persisted")
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    let transactions = workspace.join(".wheat/materialization-transactions");
    let transaction = std::fs::read_dir(&transactions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let progress_path = transaction.join("progress.log");
    let mut bytes = std::fs::read(&progress_path).unwrap();
    assert_eq!(bytes.len(), 36);
    bytes[35] ^= 0xff;
    std::fs::write(&progress_path, bytes).unwrap();

    let recovered = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(std::fs::read(workspace.join("replace")).unwrap(), b"new\n");
    assert!(!workspace.join("delete").exists());
    assert_eq!(
        std::fs::read(workspace.join("create")).unwrap(),
        b"create\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn interrupted_rematerialization_recovers_exchange_and_deletion() {
    for (checkpoint, occurrence) in [
        ("journal-created", 1),
        ("intent-persisted", 2),
        ("exchanged", 1),
        ("exchanged", 2),
        ("deletion-unlinked", 1),
        ("all-installed", 1),
    ] {
        let root = tempfile::tempdir().unwrap();
        let stores = root.path().join("stores");
        let source = source_with_delta(root.path());
        let workspace = root.path().join("workspace");
        assert!(
            wheat(
                &stores,
                &[
                    "import",
                    source.to_str().unwrap(),
                    workspace.to_str().unwrap(),
                    "--branch",
                    "refs/heads/main"
                ],
                None
            )
            .status
            .success()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
        let interrupted = command
            .args([
                "--workspace",
                workspace.to_str().unwrap(),
                "materialize",
                "--source",
                "branch:refs/heads/feature",
            ])
            .env("WHEAT_STORE_HOME", &stores)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
            .env(
                "WHEAT_TEST_MATERIALIZATION_FAIL_OCCURRENCE",
                occurrence.to_string(),
            )
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
            .output()
            .unwrap();
        assert!(!interrupted.status.success(), "checkpoint {checkpoint}");
        let recovered = wheat(
            &stores,
            &["--workspace", workspace.to_str().unwrap(), "status"],
            None,
        );
        assert!(
            recovered.status.success(),
            "checkpoint {checkpoint}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(std::fs::read(workspace.join("replace")).unwrap(), b"new\n");
        assert!(!workspace.join("delete").exists());
        assert_eq!(
            std::fs::read(workspace.join("create")).unwrap(),
            b"create\n"
        );
        assert!(!workspace.join(".wheat/materialization.json").exists());
    }
}

#[test]
fn rematerialization_recovery_refuses_create_collision_and_toctou_change() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    assert!(
        !wheat(
            &stores,
            &[
                "--workspace",
                workspace.to_str().unwrap(),
                "materialize",
                "--source",
                "branch:refs/heads/feature"
            ],
            Some("journal-created")
        )
        .status
        .success()
    );
    std::fs::write(workspace.join("create"), b"collision\n").unwrap();
    std::fs::write(workspace.join("replace"), b"raced\n").unwrap();
    let blocked = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(workspace.join("create")).unwrap(),
        b"collision\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"raced\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("delete")).unwrap(),
        b"delete\n"
    );
}

#[test]
fn rematerialization_recovery_refuses_replacement_toctou() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    assert!(
        !wheat(
            &stores,
            &[
                "--workspace",
                workspace.to_str().unwrap(),
                "materialize",
                "--source",
                "branch:refs/heads/feature"
            ],
            Some("journal-created")
        )
        .status
        .success()
    );
    std::fs::write(workspace.join("replace"), b"raced\n").unwrap();

    let blocked = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"raced\n"
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["selection"]["name"], "refs/heads/main");
    assert!(workspace.join(".wheat/materialization.json").is_file());
}

#[test]
fn concurrent_writer_between_authentication_and_exchange_is_restored() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let pause = root.path().join("pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        "before-exchange",
        1,
        &pause,
    );
    let reached = pause.with_extension("reached");
    while !reached.exists() {
        thread::sleep(Duration::from_millis(2));
    }
    std::fs::write(workspace.join("delete"), b"concurrent\n").unwrap();
    let output = release_pause(child, &pause);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(workspace.join("delete")).unwrap(),
        b"concurrent\n"
    );
    let transactions = workspace.join(".wheat/materialization-transactions");
    let transaction = std::fs::read_dir(transactions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(std::fs::read_dir(transaction).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("leaf-")
    }));
}

#[test]
fn concurrent_writer_between_authentication_and_replacement_exchange_is_restored() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let pause = root.path().join("replacement-pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        "before-exchange",
        2,
        &pause,
    );
    let reached = pause.with_extension("reached");
    while !reached.exists() {
        thread::sleep(Duration::from_millis(2));
    }
    std::fs::write(workspace.join("replace"), b"concurrent replacement\n").unwrap();
    let output = release_pause(child, &pause);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"concurrent replacement\n"
    );
}

#[test]
fn changed_workspace_after_exchange_preserves_both_values() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let pause = root.path().join("post-exchange-pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        "exchanged",
        2,
        &pause,
    );
    let reached = pause.with_extension("reached");
    while !reached.exists() {
        thread::sleep(Duration::from_millis(2));
    }
    let transaction = std::fs::read_dir(workspace.join(".wheat/materialization-transactions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let displaced = std::fs::read_dir(&transaction)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| std::fs::read(path).ok().as_deref() == Some(b"old\n"))
        .expect("replacement preimage in private transaction");
    std::fs::write(&displaced, b"concurrent preimage\n").unwrap();
    std::fs::write(workspace.join("replace"), b"concurrent workspace\n").unwrap();
    let output = release_pause(child, &pause);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"concurrent workspace\n"
    );
    assert_eq!(std::fs::read(displaced).unwrap(), b"concurrent preimage\n");
}

#[test]
fn concurrent_writer_between_marker_authentication_and_move_is_restored() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let pause = root.path().join("move-pause");
    let child = paused_wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        "before-deletion-move",
        1,
        &pause,
    );
    let reached = pause.with_extension("reached");
    while !reached.exists() {
        thread::sleep(Duration::from_millis(2));
    }
    std::fs::write(workspace.join("delete"), b"concurrent move\n").unwrap();
    let output = release_pause(child, &pause);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(workspace.join("delete")).unwrap(),
        b"concurrent move\n"
    );
}

#[test]
fn published_metadata_allows_workspace_edits_while_private_debris_is_cleaned() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let interrupted = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/feature",
        ],
        Some("target-published"),
    );
    assert!(!interrupted.status.success());
    std::fs::write(workspace.join("replace"), b"post-commit edit\n").unwrap();
    let recovered = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        std::fs::read(workspace.join("replace")).unwrap(),
        b"post-commit edit\n"
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn no_op_materialization_does_not_rewrite_workspace_or_metadata() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source_with_delta(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
                "--branch",
                "refs/heads/main"
            ],
            None
        )
        .status
        .success()
    );
    let file = std::fs::metadata(workspace.join("same")).unwrap();
    let metadata = std::fs::read(workspace.join(".wheat/workspace.json")).unwrap();
    let noop = wheat(
        &stores,
        &[
            "--workspace",
            workspace.to_str().unwrap(),
            "materialize",
            "--source",
            "branch:refs/heads/main",
        ],
        None,
    );
    assert!(
        noop.status.success(),
        "{}",
        String::from_utf8_lossy(&noop.stderr)
    );
    assert_eq!(
        file.ino(),
        std::fs::metadata(workspace.join("same")).unwrap().ino()
    );
    assert_eq!(
        metadata,
        std::fs::read(workspace.join(".wheat/workspace.json")).unwrap()
    );
    assert!(!workspace.join(".wheat/materialization.json").exists());
}

#[test]
fn abrupt_transaction_creation_windows_are_cleaned_on_recovery() {
    for checkpoint in ["transaction-directory-created", "progress-created"] {
        let root = tempfile::tempdir().unwrap();
        let stores = root.path().join("stores");
        let source = source(root.path());
        let workspace = root.path().join("workspace");
        let failed = wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap(),
            ],
            Some(checkpoint),
        );
        assert!(!failed.status.success(), "checkpoint {checkpoint}");
        assert!(!workspace.join(".wheat/materialization.json").exists());
        assert_eq!(
            std::fs::read_dir(workspace.join(".wheat/materialization-transactions"))
                .unwrap()
                .count(),
            1
        );

        let recovered = wheat(
            &stores,
            &["--workspace", workspace.to_str().unwrap(), "status"],
            None,
        );
        assert!(!recovered.status.success());
        assert!(
            String::from_utf8_lossy(&recovered.stderr)
                .contains("workspace has not been materialized"),
            "checkpoint {checkpoint}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            std::fs::read_dir(workspace.join(".wheat/materialization-transactions"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn recovery_removes_only_authenticated_empty_orphan_transactions() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source(root.path());
    let workspace = root.path().join("workspace");
    assert!(
        wheat(
            &stores,
            &[
                "import",
                source.to_str().unwrap(),
                workspace.to_str().unwrap()
            ],
            None
        )
        .status
        .success()
    );
    let transactions = workspace.join(".wheat/materialization-transactions");
    std::fs::create_dir_all(&transactions).unwrap();
    let empty = transactions.join("018f47d2-24ab-7b30-b764-5c9259b7f209");
    std::fs::create_dir(&empty).unwrap();
    let removable = transactions.join("018f47d2-24ab-7b30-b764-5c9259b7f20a");
    std::fs::create_dir(&removable).unwrap();
    std::fs::write(removable.join("progress.log"), b"").unwrap();
    let status = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!empty.exists());
    assert!(!removable.exists());

    let preserved = transactions.join("018f47d2-24ab-7b30-b764-5c9259b7f20b");
    std::fs::create_dir(&preserved).unwrap();
    std::fs::write(preserved.join("progress.log"), b"").unwrap();
    std::fs::write(preserved.join("unknown"), b"private?\n").unwrap();
    let blocked = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(
        std::fs::read(preserved.join("unknown")).unwrap(),
        b"private?\n"
    );
}

#[test]
fn recovery_preserves_a_concurrent_workspace_collision() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = source(root.path());
    let workspace = root.path().join("workspace");
    let failed = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
        Some("journal-created"),
    );
    assert!(!failed.status.success());
    std::fs::write(workspace.join("a"), b"external\n").unwrap();

    let blocked = wheat(
        &stores,
        &["--workspace", workspace.to_str().unwrap(), "status"],
        None,
    );
    assert!(!blocked.status.success());
    assert_eq!(std::fs::read(workspace.join("a")).unwrap(), b"external\n");
    assert!(workspace.join(".wheat/materialization.json").is_file());
}
