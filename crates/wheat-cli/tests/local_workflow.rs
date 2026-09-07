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
        &[
            "change",
            "new",
            "--path",
            "shared",
            "--value",
            "base",
            "--replacement-kind",
            "executable",
        ],
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
            "--expected-kind",
            "executable",
            "--value",
            "child",
        ],
    );
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let b_change = field(&b, "change");
    let b_revision = field(&b, "revision");

    // Revision capture reads only the explicitly selected workspace file.
    let shared = directory.path().join("shared");
    std::fs::write(&shared, "revised-base").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&shared, {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(0o755)
    })
    .unwrap();
    let revised = wheat(
        directory.path(),
        &[
            "add",
            "--change",
            &a_change,
            "--predecessor",
            &a_revision,
            "shared",
        ],
    );
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stderr)
    );
    let revised_edits = wheat(
        directory.path(),
        &["edits", "--revision", &field(&revised, "revision")],
    );
    assert!(
        revised_edits.status.success(),
        "{}",
        String::from_utf8_lossy(&revised_edits.stderr)
    );
    // The predecessor's initial whole-file capture cannot be normalized into
    // guarded components, so the incremental transition has no subsumption claim.
    assert!(
        String::from_utf8_lossy(&revised_edits.stdout)
            .contains("transition preserved=0 added=1 removed=1 subsumed=0")
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

    // Omitting --kind preserves the target tree's executable kind. A follow-up
    // expecting that kind therefore applies cleanly.
    let follow_up = wheat(
        directory.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &b_change,
            "--path",
            "shared",
            "--expected",
            "resolved",
            "--expected-kind",
            "executable",
            "--value",
            "after-resolution",
        ],
    );
    assert!(
        follow_up.status.success(),
        "{}",
        String::from_utf8_lossy(&follow_up.stderr)
    );
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
fn change_revise_is_rejected_and_add_help_requires_explicit_paths() {
    let dir = tempfile::tempdir().unwrap();
    let revised = wheat(dir.path(), &["change", "revise"]);
    assert!(!revised.status.success());
    assert!(String::from_utf8_lossy(&revised.stderr).contains("unrecognized subcommand 'revise'"));

    let change_help = wheat(dir.path(), &["change", "--help"]);
    assert!(change_help.status.success());
    let change_help = String::from_utf8_lossy(&change_help.stdout);
    assert!(change_help.contains("new"));
    assert!(!change_help.contains("revise"));

    let add_help = wheat(dir.path(), &["add", "--help"]);
    assert!(add_help.status.success());
    let add_help = String::from_utf8_lossy(&add_help.stdout);
    assert!(add_help.contains("--change"));
    assert!(add_help.contains("--predecessor"));
    assert!(add_help.contains("Explicit repository-relative workspace paths"));
}

#[test]
fn string_values_are_normalized_and_mode_only_edits_preserve_content() {
    let dir = tempfile::tempdir().unwrap();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let created = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one\r\ntwo\r"],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let normalized = b"one\ntwo\n";
    let content_id = wheat_core::ContentId::of(normalized).to_string();
    let retained = wheat(dir.path(), &["cat", &content_id]);
    assert!(retained.status.success());
    assert_eq!(retained.stdout, normalized);

    let mode_only = wheat(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&created, "change"),
            "--path",
            "file",
            "--expected",
            "one\r\ntwo\r",
            "--expected-kind",
            "regular",
            "--replacement-kind",
            "executable",
        ],
    );
    assert!(
        mode_only.status.success(),
        "{}",
        String::from_utf8_lossy(&mode_only.stderr)
    );
    assert_eq!(wheat(dir.path(), &["cat", &content_id]).stdout, normalized);
}

#[test]
fn symlink_string_values_preserve_carriage_returns() {
    let dir = tempfile::tempdir().unwrap();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let target = "directory\r\ntarget\r";
    let created = wheat(
        dir.path(),
        &[
            "change",
            "new",
            "--path",
            "link",
            "--value",
            target,
            "--replacement-kind",
            "symlink",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let content_id = wheat_core::ContentId::of(target.as_bytes()).to_string();
    assert_eq!(
        wheat(dir.path(), &["cat", &content_id]).stdout,
        target.as_bytes()
    );

    let changed = wheat(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&created, "change"),
            "--path",
            "link",
            "--expected",
            target,
            "--expected-kind",
            "symlink",
            "--value",
            "next\rtarget",
            "--replacement-kind",
            "symlink",
        ],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let replacement_id = wheat_core::ContentId::of(b"next\rtarget").to_string();
    assert_eq!(
        wheat(dir.path(), &["cat", &replacement_id]).stdout,
        b"next\rtarget"
    );
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
