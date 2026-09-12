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

fn wheat(workspace: &Path, arguments: &[&str]) -> std::process::Output {
    let mut arguments = arguments.to_vec();
    if arguments.starts_with(&["change", "new"]) {
        arguments.extend(["--message", "test revision"]);
    }
    let config_home = workspace.parent().unwrap().join("wheat-test-config");
    std::fs::create_dir_all(config_home.join("wheat")).unwrap();
    std::fs::write(
        config_home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .env(
            "WHEAT_STORE_HOME",
            workspace.parent().unwrap().join("stores"),
        )
        .env("XDG_CONFIG_HOME", config_home)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

fn field(output: &std::process::Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

#[test]
fn imported_history_pushes_native_revision_as_a_fast_forward_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let remote = root.path().join("remote.git");
    let wheat_repository = root.path().join("wheat");
    let clone = root.path().join("clone");
    git(None, &["init", "-b", "main", source.to_str().unwrap()]);
    std::fs::write(source.join("base"), b"base\r\n").unwrap();
    git(Some(&source), &["add", "base"]);
    git(Some(&source), &["commit", "-m", "base"]);
    let original = git(Some(&source), &["rev-parse", "HEAD"]);
    git(None, &["init", "--bare", remote.to_str().unwrap()]);
    git(
        Some(&source),
        &["push", remote.to_str().unwrap(), "HEAD:refs/heads/main"],
    );

    let imported = wheat(
        &wheat_repository,
        &[
            "import",
            source.to_str().unwrap(),
            wheat_repository.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let created = wheat(
        &wheat_repository,
        &["change", "new", "--path", "native", "--value", "from wheat"],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let revision = field(&created, "revision");

    let pushed = wheat(
        &wheat_repository,
        &["push", remote.to_str().unwrap(), "--to", "refs/heads/main"],
    );
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert!(String::from_utf8_lossy(&pushed.stdout).contains("pushed 1 Wheat revisions"));

    let projected = git(Some(&remote), &["rev-parse", "refs/heads/main"]);
    assert_ne!(projected, original);
    assert_eq!(
        git(Some(&remote), &["rev-parse", "refs/heads/main^"]),
        original
    );
    git(
        None,
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(std::fs::read(clone.join("base")).unwrap(), b"base\n");
    assert_eq!(std::fs::read(clone.join("native")).unwrap(), b"from wheat");
    assert_eq!(git(Some(&clone), &["fsck", "--strict"]), "");

    let log = wheat(&wheat_repository, &["log"]);
    assert!(log.status.success());
    assert!(String::from_utf8_lossy(&log.stdout).contains(&revision));
    let repeated = wheat(
        &wheat_repository,
        &["push", remote.to_str().unwrap(), "--to", "refs/heads/main"],
    );
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(
        git(Some(&remote), &["rev-parse", "refs/heads/main"]),
        projected
    );
}

#[test]
fn unknown_remote_tip_is_rejected_without_force_with_lease() {
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("repository");
    let remote_work = root.path().join("remote-work");
    let remote = root.path().join("remote.git");
    assert!(
        wheat(&repository, &["init", repository.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        wheat(
            &repository,
            &["change", "new", "--path", "native", "--value", "wheat"],
        )
        .status
        .success()
    );
    git(None, &["init", "-b", "main", remote_work.to_str().unwrap()]);
    std::fs::write(remote_work.join("foreign"), b"foreign\n").unwrap();
    git(Some(&remote_work), &["add", "foreign"]);
    git(Some(&remote_work), &["commit", "-m", "foreign"]);
    let foreign = git(Some(&remote_work), &["rev-parse", "HEAD"]);
    git(None, &["init", "--bare", remote.to_str().unwrap()]);
    git(
        Some(&remote_work),
        &["push", remote.to_str().unwrap(), "HEAD:refs/heads/main"],
    );

    let rejected = wheat(
        &repository,
        &["push", remote.to_str().unwrap(), "--to", "refs/heads/main"],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unknown commit"));
    assert_eq!(
        git(Some(&remote), &["rev-parse", "refs/heads/main"]),
        foreign
    );

    let forced = wheat(
        &repository,
        &[
            "push",
            remote.to_str().unwrap(),
            "--to",
            "refs/heads/main",
            "--force-with-lease",
        ],
    );
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_ne!(
        git(Some(&remote), &["rev-parse", "refs/heads/main"]),
        foreign
    );
}
