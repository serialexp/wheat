use std::{path::Path, process::Command, time::Duration};

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

fn wheat(stores: &Path, args: &[&str], checkpoint: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
    command.args(args).env("WHEAT_STORE_HOME", stores);
    if let Some(checkpoint) = checkpoint {
        command.env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint);
    }
    command.output().unwrap()
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
    let started = std::time::Instant::now();
    let failed = wheat(
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
        Some("all-installed"),
    );
    let elapsed = started.elapsed();
    assert!(!failed.status.success());
    assert!(
        elapsed < Duration::from_secs(30),
        "1,000-file import and materialization took {elapsed:?}"
    );

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
