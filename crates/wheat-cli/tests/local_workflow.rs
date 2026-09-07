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
    assert!(directory.path().join(".wheat/repository.head").is_file());
    assert!(!directory.path().join(".wheat/repository.yaml").exists());

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
    let content_id = wheat_core::ContentId::of(b"world").to_string();
    let object = directory
        .path()
        .join(".wheat/content")
        .join(&content_id[..2])
        .join(&content_id[2..]);
    assert!(object.is_file());
    let retained = wheat(directory.path(), &["cat", &content_id]);
    assert!(retained.status.success());
    assert_eq!(retained.stdout, b"world");

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

#[test]
fn legacy_yaml_is_rejected_without_modification() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".wheat")).unwrap();
    let path = dir.path().join(".wheat/repository.yaml");
    std::fs::write(&path, b"legacy snapshot").unwrap();
    for command in ["init", "status"] {
        let result = wheat(dir.path(), &[command]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("legacy YAML"));
    }
    assert_eq!(std::fs::read(path).unwrap(), b"legacy snapshot");
    assert!(!dir.path().join(".wheat/repository.head").exists());
}

#[test]
fn failed_mutation_keeps_head_and_emits_no_success() {
    let dir = tempfile::tempdir().unwrap();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let created = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(created.status.success());
    let path = dir.path().join(".wheat/repository.head");
    let before = std::fs::read(&path).unwrap();
    let failed = wheat(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&created, "change"),
            "--path",
            "file",
            "--expected",
            "wrong",
            "--value",
            "two",
        ],
    );
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(wheat(dir.path(), &["status"]).status.success());
}

#[test]
fn corrupted_canonical_state_is_not_replaced() {
    use wheat_store::{LocalPublicationStore, LooseObjectStore, PublicationStore};
    let dir = tempfile::tempdir().unwrap();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let head_path = dir.path().join(".wheat/repository.head");
    let before = std::fs::read(&head_path).unwrap();
    let head = LocalPublicationStore::new(&head_path).read().unwrap();
    let objects = LooseObjectStore::new(dir.path().join(".wheat/objects"));
    std::fs::write(objects.path(head.envelope.into()), b"corrupt").unwrap();
    let result = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(std::fs::read(head_path).unwrap(), before);
}
