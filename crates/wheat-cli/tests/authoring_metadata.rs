#![cfg(target_os = "linux")]

use std::{
    path::Path,
    process::{Command, Output},
};

use wheat_store::{
    ImmutableObjectStore, LocalPublicationStore, LooseObjectStore, ObjectKind, PublicationStore,
    RepositoryReader, RetainedSetStore,
};

fn command(root: &Path, config_home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(root)
        .args(arguments)
        .env("WHEAT_STORE_HOME", root.parent().unwrap().join("stores"))
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .unwrap()
}

fn ok(root: &Path, config_home: &Path, arguments: &[&str]) -> Output {
    let output = command(root, config_home, arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn store_path(root: &Path) -> std::path::PathBuf {
    std::fs::read_dir(root.parent().unwrap().join("stores"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}

async fn records(
    root: &Path,
    revision: wheat_core::RevisionId,
) -> (wheat_core::RevisionRecord, wheat_core::Change) {
    let store = store_path(root);
    let objects = LooseObjectStore::new(store.join("objects"));
    let content = wheat_store::ContentStore::new(store.join("content"));
    let heads = LocalPublicationStore::new(store.join("repository.head"));
    let reader = RepositoryReader::new(&objects, &content);
    let head = heads.read().unwrap();
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let state = reader.state(envelope.state).await.unwrap();
    let revision = reader.revision(revision).await.unwrap();
    let retained = RetainedSetStore::new(&objects);
    let mut cursor = retained.cursor(state.retained_changes).await.unwrap();
    while let Some(id) = cursor.next().await.unwrap() {
        let object = objects.get(id).await.unwrap();
        if object.kind == ObjectKind::Change {
            let change = wheat_core::decode_change(&object.canonical_bytes).unwrap();
            if change.id == revision.change_id {
                return (revision, change);
            }
        }
    }
    panic!("change record missing")
}

#[tokio::test]
async fn repository_config_overrides_global_and_metadata_round_trips() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let config_home = parent.path().join("config");
    ok(&root, &config_home, &["init"]);
    ok(
        &root,
        &config_home,
        &["config", "set", "--global", "actor.name", "Global Name"],
    );
    ok(
        &root,
        &config_home,
        &[
            "config",
            "set",
            "--global",
            "actor.email",
            "global@example.invalid",
        ],
    );
    ok(
        &root,
        &config_home,
        &["config", "set", "actor.name", "Repository Name"],
    );

    let created = ok(
        &root,
        &config_home,
        &[
            "change",
            "new",
            "--message",
            "Stable title\n\nRevision body",
            "--description",
            "Change body",
            "--created-at",
            "2026-09-01T01:02:03-04:00",
            "--authored-at",
            "2026-09-02T02:03:04+05:30",
            "--committed-at",
            "2026-09-03T03:04:05Z",
            "--path",
            "file",
            "--value",
            "one",
        ],
    );
    let revision: wheat_core::RevisionId = field(&created, "revision").parse().unwrap();
    let (revision_record, change) = records(&root, revision).await;
    assert_eq!(change.metadata.title, "Stable title");
    assert_eq!(change.metadata.description, "Change body");
    assert_eq!(change.metadata.creator.name, "Repository Name");
    assert_eq!(change.metadata.creator.email, "global@example.invalid");
    assert_eq!(change.metadata.created_at.offset_minutes, -240);
    assert_eq!(
        revision_record.metadata.message,
        "Stable title\n\nRevision body"
    );
    assert_eq!(revision_record.metadata.author, change.metadata.creator);
    assert_eq!(revision_record.metadata.committer, change.metadata.creator);
    assert_eq!(revision_record.metadata.authored_at.offset_minutes, 330);
    assert_eq!(revision_record.metadata.committed_at.offset_minutes, 0);

    let log = ok(&root, &config_home, &["log"]);
    let log = String::from_utf8(log.stdout).unwrap();
    assert!(log.contains("Stable title"));
    assert!(
        log.contains("author Repository Name <global@example.invalid> 2026-09-02T02:03:04+05:30")
    );
    assert!(log.contains("Revision body"));

    let log = ok(&root, &config_home, &["log", "--format", "json"]);
    let log: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    let entry = &log["result"]["entries"][0];
    assert_eq!(entry["change_id"], change.id.to_string());
    assert_eq!(entry["change"]["title"], "Stable title");
    assert_eq!(entry["change"]["description"], "Change body");
    assert_eq!(entry["change"]["created_at"]["seconds"], 1_788_238_923_i64);
    assert_eq!(entry["change"]["created_at"]["offset_minutes"], -240);
    assert_eq!(entry["revision_id"], revision.to_string());
    assert!(entry["predecessor"].is_null());
    assert_eq!(entry["revision"]["author"]["name"], "Repository Name");
    assert_eq!(
        entry["revision"]["author"]["email"],
        "global@example.invalid"
    );
    assert_eq!(entry["revision"]["authored_at"]["offset_minutes"], 330);
    assert_eq!(entry["revision"]["committed_at"]["offset_minutes"], 0);
    assert_eq!(
        entry["revision"]["message"],
        "Stable title\n\nRevision body"
    );
    assert!(log["result"].get("text").is_none());
}

#[test]
fn authoring_refuses_missing_identity_message_and_invalid_configuration() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let config_home = parent.path().join("config");
    ok(&root, &config_home, &["init"]);

    let missing = command(
        &root,
        &config_home,
        &[
            "change",
            "new",
            "--message",
            "message",
            "--path",
            "file",
            "--value",
            "one",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("actor name is not configured"));
    assert!(missing.stdout.is_empty());

    let missing_message = command(
        &root,
        &config_home,
        &["change", "new", "--path", "file", "--value", "one"],
    );
    assert!(!missing_message.status.success());
    assert!(String::from_utf8_lossy(&missing_message.stderr).contains("--message"));

    let bad_key = command(&root, &config_home, &["config", "set", "unknown", "value"]);
    assert!(!bad_key.status.success());
    let bad_email = command(
        &root,
        &config_home,
        &["config", "set", "actor.email", "invalid"],
    );
    assert!(!bad_email.status.success());
    assert!(String::from_utf8_lossy(&bad_email.stderr).contains("local part and domain"));
    assert!(!store_path(&root).join("config.json").exists());
}

#[tokio::test]
async fn add_records_new_revision_metadata_without_changing_change_metadata() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let config_home = parent.path().join("config");
    ok(&root, &config_home, &["init"]);
    ok(
        &root,
        &config_home,
        &["config", "set", "actor.name", "Author"],
    );
    ok(
        &root,
        &config_home,
        &["config", "set", "actor.email", "author@example.invalid"],
    );
    let created = ok(
        &root,
        &config_home,
        &[
            "change",
            "new",
            "--message",
            "Original title",
            "--created-at",
            "2026-01-01T00:00:00Z",
            "--path",
            "file",
            "--value",
            "one",
        ],
    );
    let change = field(&created, "change");
    let predecessor = field(&created, "revision");
    ok(&root, &config_home, &["materialize"]);
    std::fs::write(root.join("file"), b"two").unwrap();
    let revised = ok(
        &root,
        &config_home,
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "--message",
            "Second revision",
            "--authored-at",
            "2026-02-01T00:00:00+01:00",
            "file",
        ],
    );
    let revision: wheat_core::RevisionId = field(&revised, "revision").parse().unwrap();
    let (revision, change_record) = records(&root, revision).await;
    assert_eq!(change_record.metadata.title, "Original title");
    assert_eq!(change_record.metadata.created_at.offset_minutes, 0);
    assert_eq!(revision.metadata.message, "Second revision");
    assert_eq!(revision.metadata.authored_at.offset_minutes, 60);
}
