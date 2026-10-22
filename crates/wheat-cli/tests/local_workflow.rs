#![cfg(target_os = "linux")]

use std::process::{Command, Output};

use sandbox::sandbox;
use wheat_store::{LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryReader};

mod sandbox;
mod support;

fn wheat(root: &std::path::Path, arguments: &[&str]) -> Output {
    let mut adjusted = arguments.to_vec();
    if adjusted.first() == Some(&"init") && adjusted.len() == 1 {
        adjusted.push(root.to_str().unwrap());
    }
    add_test_message(&mut adjusted);
    let config_home = root.parent().unwrap().join(format!(
        ".{}-test-config",
        root.file_name().unwrap().to_string_lossy()
    ));
    write_test_actor(&config_home);
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(root)
        .args(adjusted)
        .env("WHEAT_STORE_HOME", store_home(root))
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .expect("wheat command runs")
}

fn add_test_message(arguments: &mut Vec<&str>) {
    let is_new = arguments.starts_with(&["change", "new"]);
    let is_add = arguments.first() == Some(&"add");
    if (is_new || is_add)
        && !arguments
            .iter()
            .any(|argument| *argument == "--message" || *argument == "-m")
    {
        arguments.extend(["--message", "test revision"]);
    }
}

fn write_test_actor(config_home: &std::path::Path) {
    let directory = config_home.join("wheat");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
}

fn store_home(root: &std::path::Path) -> std::path::PathBuf {
    root.parent().unwrap().join(format!(
        ".{}-test-stores",
        root.file_name().unwrap().to_string_lossy()
    ))
}

fn store_path(root: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(store_home(root))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .expect("output field exists")
        .to_owned()
}

fn packed_catalog_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root.join("storage/packed/catalog"))
        .unwrap()
        .count()
}

fn git(root: &std::path::Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git command runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn initializes_mutates_and_reopens_local_repository() {
    let directory = sandbox();

    let initialized = wheat(directory.path(), &["init"]);
    assert!(initialized.status.success());
    assert!(directory.path().join(".wheat/workspace.json").is_file());
    assert!(
        store_path(directory.path())
            .join("repository.head")
            .is_file()
    );

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
    assert!(store_path(directory.path()).join("storage").is_dir());
    assert!(!store_path(directory.path()).join("objects").exists());
    let retained = wheat(directory.path(), &["cat", &content_id]);
    assert!(retained.status.success());
    assert_eq!(retained.stdout, b"world");

    let reopened = wheat(directory.path(), &["status"]);
    assert!(reopened.status.success());
    let status = String::from_utf8_lossy(&reopened.stdout);
    assert!(status.contains("active label: main"));
    assert!(status.contains("tracked changes: 0"));
    let validated = wheat(directory.path(), &["validate"]);
    assert!(
        validated.status.success(),
        "{}",
        String::from_utf8_lossy(&validated.stderr)
    );
    assert!(String::from_utf8_lossy(&validated.stdout).contains("valid state: "));

    let duplicate = wheat(directory.path(), &["init"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("not empty"));
}

#[test]
fn inspects_and_mutates_an_atomic_packed_repository() {
    let git_repository = tempfile::tempdir().unwrap();
    git(git_repository.path(), &["init", "-b", "main"]);
    std::fs::write(git_repository.path().join("file"), b"one\n").unwrap();
    git(git_repository.path(), &["add", "file"]);
    git(git_repository.path(), &["commit", "-m", "root"]);

    let parent = tempfile::tempdir().unwrap();
    let repository = parent.path().join("atomic");
    let imported = wheat(
        &repository,
        &[
            "import",
            git_repository.path().to_str().unwrap(),
            repository.to_str().unwrap(),
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let store = store_path(&repository);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let branches_before = {
        let objects = PackedObjectStore::open(store.join("storage")).unwrap();
        let content = objects.content_store();
        let heads = LocalPublicationStore::new(store.join("repository.head"));
        let reader = RepositoryReader::new(&objects, &content);
        runtime.block_on(async {
            let envelope = reader
                .envelope(heads.read().unwrap().envelope)
                .await
                .unwrap();
            reader.view_label_manifest(envelope.labels).await.unwrap()
        })
    };
    assert!(branches_before.labels.contains_key("main"));

    let status = wheat(&repository, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(String::from_utf8_lossy(&status.stdout).contains("active label: main"));

    let catalogs_before = packed_catalog_count(&store);
    let created = wheat(
        &repository,
        &["change", "new", "--path", "added", "--value", "packed"],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    // Small content and metadata batches grow the active pack. They must not turn
    // logical mutation or durability boundaries into immutable catalog generations.
    assert_eq!(packed_catalog_count(&store), catalogs_before);
    assert!(store.join("storage/packed/active.segment").is_file());
    let reopened = wheat(&repository, &["status"]);
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("active label: main"));

    let log = wheat(&repository, &["log"]);
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    assert!(String::from_utf8_lossy(&log.stdout).contains(&field(&created, "revision")));
    let edits = wheat(
        &repository,
        &["edits", "--revision", &field(&created, "revision")],
    );
    assert!(
        edits.status.success(),
        "{}",
        String::from_utf8_lossy(&edits.stderr)
    );
    assert!(String::from_utf8_lossy(&edits.stdout).contains("added exact-file"));
    let conflicts = wheat(&repository, &["conflicts"]);
    assert!(conflicts.status.success());
    assert!(conflicts.stdout.is_empty());

    let dependent = wheat(
        &repository,
        &[
            "change",
            "new",
            "--depends-on",
            &field(&created, "change"),
            "--path",
            "added",
            "--expected",
            "packed",
            "--value",
            "dependent",
        ],
    );
    assert!(
        dependent.status.success(),
        "{}",
        String::from_utf8_lossy(&dependent.stderr)
    );
    std::fs::write(repository.join("added"), "revised").unwrap();
    let revised = wheat(
        &repository,
        &[
            "add",
            "--change",
            &field(&created, "change"),
            "--predecessor",
            &field(&created, "revision"),
            "added",
        ],
    );
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stderr)
    );
    let conflicted = wheat(&repository, &["status"]);
    assert!(String::from_utf8_lossy(&conflicted.stdout).contains("active label: main"));
    let conflicts = wheat(&repository, &["conflicts", "--format", "json"]);
    let conflicts: serde_json::Value = serde_json::from_slice(&conflicts.stdout).unwrap();
    assert_eq!(
        conflicts["result"]["conflicts"].as_array().unwrap().len(),
        1
    );
    let resolved = wheat(
        &repository,
        &[
            "resolve",
            &field(&dependent, "revision"),
            "--path",
            "added",
            "--value",
            "resolved",
        ],
    );
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let reopened = wheat(&repository, &["status"]);
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("active label: main"));
    let conflicts = wheat(&repository, &["conflicts", "--format", "json"]);
    let conflicts: serde_json::Value = serde_json::from_slice(&conflicts.stdout).unwrap();
    assert_eq!(conflicts["result"]["conflicts"], serde_json::json!([]));
    let validated = wheat(&repository, &["validate"]);
    assert!(
        validated.status.success(),
        "{}",
        String::from_utf8_lossy(&validated.stderr)
    );

    let branches_after = {
        let objects = PackedObjectStore::open(store.join("storage")).unwrap();
        let content = objects.content_store();
        let heads = LocalPublicationStore::new(store.join("repository.head"));
        let reader = RepositoryReader::new(&objects, &content);
        runtime.block_on(async {
            let envelope = reader
                .envelope(heads.read().unwrap().envelope)
                .await
                .unwrap();
            reader.view_label_manifest(envelope.labels).await.unwrap()
        })
    };
    assert_eq!(branches_after, branches_before);

    let content_id = wheat_core::ContentId::of(b"packed").to_string();
    let retained = wheat(&repository, &["cat", &content_id]);
    assert!(
        retained.status.success(),
        "{}",
        String::from_utf8_lossy(&retained.stderr)
    );
    assert_eq!(retained.stdout, b"packed");
}

#[test]
fn revises_a_stack_and_resolves_a_conflict_after_reopening() {
    let directory = sandbox();
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
    assert!(String::from_utf8_lossy(&conflicted.stdout).contains("active label: main"));
    let conflicts = wheat(directory.path(), &["conflicts"]);
    assert!(conflicts.status.success());
    assert!(String::from_utf8_lossy(&conflicts.stdout).contains("path=\"shared\""));
    let conflicts_json_output = wheat(directory.path(), &["conflicts", "--format", "json"]);
    assert!(conflicts_json_output.status.success());
    let conflicts_json: serde_json::Value =
        serde_json::from_slice(&conflicts_json_output.stdout).unwrap();
    let conflicts_json = &conflicts_json["result"];
    assert_eq!(conflicts_json["schema_version"], 1);
    assert!(conflicts_json["head_generation"].as_u64().is_some());
    let conflict = &conflicts_json["conflicts"][0];
    assert_eq!(conflict["id"].as_array().unwrap().len(), 32);
    assert_eq!(conflict["base_view"].as_array().unwrap().len(), 32);
    assert_eq!(conflict["change_id"], b_change);
    assert_eq!(
        conflict["revision_id"],
        serde_json::to_value(b_revision.parse::<wheat_core::RevisionId>().unwrap()).unwrap()
    );
    assert_eq!(conflict["application_id"].as_array().unwrap().len(), 32);
    let edit = &conflict["edits"][0];
    assert_eq!(edit["id"].as_array().unwrap().len(), 32);
    assert_eq!(edit["path"], "shared");
    assert!(!edit["reason"].is_null());
    assert!(!edit["target"].is_null());
    assert!(edit.get("range").is_none());
    assert!(edit.get("read").is_none());
    assert!(edit.get("write").is_none());

    // A separate process reopening the durable store must see byte-identical,
    // deterministically ordered conflict output.
    let reopened_conflicts = wheat(directory.path(), &["conflicts", "--format", "json"]);
    assert!(reopened_conflicts.status.success());
    assert_eq!(reopened_conflicts.stdout, conflicts_json_output.stdout);

    let head_path = store_home(directory.path())
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("repository.head");
    let head_before_refusal = std::fs::read(&head_path).unwrap();
    let absent = wheat(
        directory.path(),
        &[
            "--format",
            "json",
            "resolve",
            "--path",
            "not-conflicted",
            "--value",
            "unreachable",
        ],
    );
    assert!(!absent.status.success());
    let absent: serde_json::Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert_eq!(absent["error"]["category"], "conflict");
    assert_eq!(std::fs::read(&head_path).unwrap(), head_before_refusal);

    let resolved = wheat(
        directory.path(),
        &["resolve", "--path", "shared", "--value", "resolved"],
    );
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let reopened = wheat(directory.path(), &["status"]);
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("active label: main"));
    let resolved_conflicts = wheat(directory.path(), &["conflicts", "--format", "json"]);
    assert!(resolved_conflicts.status.success());
    let resolved_conflicts: serde_json::Value =
        serde_json::from_slice(&resolved_conflicts.stdout).unwrap();
    assert_eq!(
        resolved_conflicts["result"]["conflicts"],
        serde_json::json!([])
    );

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
fn embedded_prototype_is_rejected_without_modification() {
    let dir = sandbox();
    std::fs::create_dir(dir.path().join(".wheat")).unwrap();
    let path = dir.path().join(".wheat/repository.yaml");
    std::fs::write(&path, b"legacy snapshot").unwrap();
    let result = wheat(dir.path(), &["status"]);
    assert!(!result.status.success());
    assert!(!result.stderr.is_empty());
    assert_eq!(std::fs::read(path).unwrap(), b"legacy snapshot");
}

#[test]
fn change_revise_is_rejected_and_add_help_requires_explicit_paths() {
    let dir = sandbox();
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
    assert!(!add_help.contains("--predecessor"));
    assert!(add_help.contains("Explicit repository-relative workspace paths"));
}

#[test]
fn string_values_are_normalized_and_mode_only_edits_preserve_content() {
    let dir = sandbox();
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
    let dir = sandbox();
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
    let dir = sandbox();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let created = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(created.status.success());
    let path = store_path(dir.path()).join("repository.head");
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

/// An initial edit whose `--expected` file does not match says which path failed,
/// why, and how to fix it, and classifies as an input error.
#[test]
fn initial_edit_that_does_not_apply_explains_expected() {
    let dir = sandbox();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let created = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(created.status.success());
    let change = field(&created, "change");

    let cases: [(&[&str], &str); 3] = [
        (
            &["--path", "file", "--value", "two"],
            "file already exists; pass --expected with its current content to change it",
        ),
        (
            &["--path", "absent", "--expected", "one", "--value", "two"],
            "absent does not exist; omit --expected to create it",
        ),
        (
            &["--path", "file", "--expected", "wrong", "--value", "two"],
            "file does not match --expected",
        ),
    ];
    for (edit, hint) in cases {
        let mut arguments = vec!["change", "new", "--depends-on", &change];
        arguments.extend_from_slice(edit);
        let failed = wheat(dir.path(), &arguments);
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert_eq!(failed.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(hint), "{stderr}");
        assert!(
            stderr.contains("authored edits do not apply cleanly: "),
            "{stderr}"
        );
    }
    // Without --depends-on the change is created on the frontier.
    let failed = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "two"],
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert_eq!(failed.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("file already exists"), "{stderr}");

    let failed = wheat(
        dir.path(),
        &[
            "--format",
            "json",
            "change",
            "new",
            "--message",
            "test revision",
            "--depends-on",
            &change,
            "--path",
            "file",
            "--value",
            "two",
        ],
    );
    assert_eq!(failed.status.code(), Some(2));
    let envelope: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "edits_do_not_apply");
    assert!(envelope["error"]["message"].as_str().unwrap().contains(
        "file: the current file differs from the edit's expected file (the path exists)"
    ));
}

#[test]
fn corrupted_canonical_state_is_not_replaced() {
    use wheat_store::{LocalPublicationStore, PublicationStore};
    let dir = sandbox();
    assert!(wheat(dir.path(), &["init"]).status.success());
    let store = store_path(dir.path());
    let head_path = store.join("repository.head");
    let before = std::fs::read(&head_path).unwrap();
    let head = LocalPublicationStore::new(&head_path).read().unwrap();
    let envelope: wheat_core::ObjectId = head.envelope.into();
    support::corrupt_packed_record(&store, *envelope.as_bytes());
    let result = wheat(
        dir.path(),
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(std::fs::read(head_path).unwrap(), before);
}
