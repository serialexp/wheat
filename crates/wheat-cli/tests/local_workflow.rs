use std::process::{Command, Output};

fn wheat(root: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--repository")
        .arg(root)
        .args(arguments)
        .output()
        .expect("wheat command runs")
}

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .expect("output field exists")
        .to_owned()
}

#[test]
fn initializes_mutates_and_reopens_local_repository() {
    let directory = tempfile::tempdir().unwrap();

    let initialized = wheat(directory.path(), &["init"]);
    assert!(initialized.status.success());
    assert!(directory.path().join(".wheat/repository.yaml").is_file());

    let created = wheat(
        directory.path(),
        &["change", "new", "--path", "hello", "--value", "world"],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(String::from_utf8_lossy(&created.stdout).contains("change "));

    let reopened = wheat(directory.path(), &["status"]);
    assert!(reopened.status.success());
    let status = String::from_utf8_lossy(&reopened.stdout);
    assert!(status.contains("changes: 1"));
    assert!(status.contains("conflicts: 0"));

    let duplicate = wheat(directory.path(), &["init"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
}

#[test]
fn revises_a_stack_and_resolves_a_conflict_after_reopening() {
    let directory = tempfile::tempdir().unwrap();
    assert!(wheat(directory.path(), &["init"]).status.success());

    let a = wheat(
        directory.path(),
        &["change", "new", "--path", "shared", "--value", "base"],
    );
    let a_change = field(&a, "change");
    let a_revision = field(&a, "revision");
    let b = wheat(
        directory.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &a_change,
            "--path",
            "shared",
            "--expected",
            "base",
            "--value",
            "child",
        ],
    );
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let b_revision = field(&b, "revision");

    let revised = wheat(
        directory.path(),
        &[
            "change",
            "revise",
            &a_change,
            "--predecessor",
            &a_revision,
            "--path",
            "shared",
            "--value",
            "revised-base",
        ],
    );
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stderr)
    );
    let conflicted = wheat(directory.path(), &["status"]);
    assert!(String::from_utf8_lossy(&conflicted.stdout).contains("conflicts: 1"));

    let resolved = wheat(
        directory.path(),
        &[
            "resolve",
            &b_revision,
            "--path",
            "shared",
            "--value",
            "resolved",
        ],
    );
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let reopened = wheat(directory.path(), &["status"]);
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("conflicts: 0"));
}
