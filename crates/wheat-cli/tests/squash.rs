#![cfg(target_os = "linux")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryHead, RepositoryReader,
};

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let config = temp.path().join("config");
    fs::create_dir_all(config.join("wheat")).unwrap();
    fs::write(config.join("wheat/config.json"), r#"{"format":"wheat-config","version":2,"actor":{"name":"Squasher","email":"squasher@example.invalid"}}"#).unwrap();
    (temp, workspace, config)
}

fn wheat(root: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(root)
        .args(args)
        .env("WHEAT_STORE_HOME", root.parent().unwrap().join("stores"))
        .env("XDG_CONFIG_HOME", config)
        .output()
        .unwrap()
}
fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn field(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap()
        .to_owned()
}
fn initialize(root: &Path, config: &Path) {
    ok(wheat(root, config, &["init"]));
}
fn materialize(root: &Path, config: &Path) {
    ok(wheat(root, config, &["materialize"]));
}
fn json(root: &Path, config: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&ok(wheat(root, config, args))).unwrap()
}
fn workspace_store(root: &Path) -> PathBuf {
    let workspace: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".wheat/workspace.json")).unwrap()).unwrap();
    PathBuf::from(workspace["store"]["path"].as_str().unwrap())
}

fn head(store: &Path) -> RepositoryHead {
    LocalPublicationStore::new(store.join("repository.head"))
        .read()
        .unwrap()
}

fn write_remote(store: &Path, observed: RepositoryHead) {
    fs::write(
        store.join("remote.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "display_url": "wheat://example.invalid/owner/repo",
            "canonical_identity": "wheat://example.invalid/owner/repo",
            "head": {
                "generation": observed.generation,
                "envelope": observed.envelope.to_string(),
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn revision(store: &Path, id: &str) -> wheat_core::RevisionRecord {
    let objects = PackedObjectStore::open(store.join("storage")).unwrap();
    let content = objects.content_store();
    RepositoryReader::new(&objects, &content)
        .revision(id.parse().unwrap())
        .await
        .unwrap()
}

fn set_actor(config: &Path, name: &str, email: &str) {
    fs::write(
        config.join("wheat/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "format": "wheat-config",
            "version": 2,
            "actor": { "name": name, "email": email }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn three_checkpoints(
    workspace: &Path,
    config: &Path,
) -> (String, [String; 3], [RepositoryHead; 3]) {
    initialize(workspace, config);
    let created = ok(wheat(
        workspace,
        config,
        &[
            "change", "new", "-m", "A1", "--path", "file", "--value", "one",
        ],
    ));
    let change = field(&created, "change ");
    let a1 = field(&created, "revision ");
    let store = workspace_store(workspace);
    let h1 = head(&store);
    materialize(workspace, config);
    fs::write(workspace.join("file"), "two").unwrap();
    let a2 = field(
        &ok(wheat(workspace, config, &["add", "--all", "-m", "A2"])),
        "revision ",
    );
    let h2 = head(&store);
    fs::write(workspace.join("file"), "three").unwrap();
    let a3 = field(
        &ok(wheat(workspace, config, &["add", "--all", "-m", "A3"])),
        "revision ",
    );
    let h3 = head(&store);
    (change, [a1, a2, a3], [h1, h2, h3])
}

#[test]
fn change_squash_collapses_checkpoints_and_preserves_edit_identity() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    let created = ok(wheat(
        &workspace,
        &config,
        &[
            "change", "new", "-m", "first", "--path", "file", "--value", "one",
        ],
    ));
    let change = field(&created, "change ");
    let first = field(&created, "revision ");
    materialize(&workspace, &config);
    fs::write(workspace.join("file"), "two").unwrap();
    let added = ok(wheat(
        &workspace,
        &config,
        &["add", "--all", "-m", "second"],
    ));
    let second = field(&added, "revision ");
    let before = ok(wheat(
        &workspace,
        &config,
        &["edits", "--revision", &second],
    ));
    let squashed = ok(wheat(
        &workspace,
        &config,
        &["--format", "json", "change", "squash", &change],
    ));
    let json: serde_json::Value = serde_json::from_str(&squashed).unwrap();
    assert_eq!(json["result"]["rewritten"], serde_json::json!([change]));
    assert_eq!(json["result"]["objects_deleted"], false);
    assert_eq!(json["result"]["gc_candidates_created"], true);
    assert!(
        json["result"]["gc_candidate_revisions"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::String(second.clone()))
    );
    assert!(
        !json["result"]["gc_candidate_views"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(json["result"]["old_objects_status"], "gc_candidates");
    assert!(json["result"]["retained_authority_count"].as_u64().unwrap() > 0);
    assert_eq!(json["result"]["consolidated_conflicts"], 0);
    assert!(!json["result"]["views"].as_object().unwrap().is_empty());
    let replacement = json["result"]["revisions"][&second].as_str().unwrap();
    assert_ne!(replacement, first);
    assert_ne!(replacement, second);
    let after = ok(wheat(
        &workspace,
        &config,
        &["edits", "--revision", replacement],
    ));
    let before_edits: Vec<_> = before
        .lines()
        .filter(|line| line.starts_with("edit "))
        .collect();
    let after_edits: Vec<_> = after
        .lines()
        .filter(|line| line.starts_with("edit "))
        .collect();
    assert_eq!(before_edits, after_edits);
    ok(wheat(&workspace, &config, &["validate"]));
}

#[test]
fn squash_is_noop_for_one_unpublished_checkpoint_and_stack_squashes_all_changes() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    ok(wheat(
        &workspace,
        &config,
        &["change", "new", "-m", "one", "--path", "a", "--value", "1"],
    ));
    let store = workspace_store(&workspace);
    let head_path = store.join("repository.head");
    let metadata_path = workspace.join(".wheat/workspace.json");
    let transaction_path = store.join("rewrite-transaction.json");
    assert!(!transaction_path.exists());
    let head_before = fs::read(&head_path).unwrap();
    let metadata_before = fs::read(&metadata_path).unwrap();
    let workspace_files_before = fs::read_dir(&workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let no_op = json(
        &workspace,
        &config,
        &["--format", "json", "change", "squash"],
    );
    assert_eq!(no_op["result"]["no_op"], true);
    assert_eq!(
        no_op["result"]["head_before"],
        no_op["result"]["head_after"]
    );
    assert_eq!(no_op["result"]["objects_deleted"], false);
    assert_eq!(no_op["result"]["gc_candidates_created"], false);
    assert!(
        no_op["result"]["gc_candidate_revisions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        no_op["result"]["gc_candidate_views"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(no_op["result"]["old_objects_status"], "unchanged");
    assert_eq!(fs::read(head_path).unwrap(), head_before);
    assert_eq!(fs::read(metadata_path).unwrap(), metadata_before);
    assert_eq!(
        fs::read_dir(&workspace)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        workspace_files_before
    );
    assert!(!transaction_path.exists());
    materialize(&workspace, &config);
    fs::write(workspace.join("a"), "2").unwrap();
    ok(wheat(&workspace, &config, &["add", "--all", "-m", "two"]));
    ok(wheat(&workspace, &config, &["restore", "a"]));
    materialize(&workspace, &config);
    ok(wheat(
        &workspace,
        &config,
        &[
            "change", "new", "-m", "child", "--path", "b", "--value", "1",
        ],
    ));
    materialize(&workspace, &config);
    fs::write(workspace.join("b"), "2").unwrap();
    ok(wheat(
        &workspace,
        &config,
        &["add", "--all", "-m", "child two"],
    ));
    let output = ok(wheat(
        &workspace,
        &config,
        &["--format", "json", "stack", "squash"],
    ));
    let json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["result"]["rewritten"].as_array().unwrap().len(), 2);
    assert!(!json["result"]["revisions"].as_object().unwrap().is_empty());
    assert!(!json["result"]["views"].as_object().unwrap().is_empty());
    assert!(!json["result"]["reapplied"].as_array().unwrap().is_empty());
    assert_eq!(json["result"]["objects_deleted"], false);
    assert_eq!(json["result"]["gc_candidates_created"], true);
    assert_eq!(json["result"]["old_objects_status"], "gc_candidates");
    assert!(json["result"]["retained_authority_count"].as_u64().unwrap() > 0);
    ok(wheat(&workspace, &config, &["validate"]));
}

/// The common local flow: `change new -m`, then several `wheat add`s that touch the same line.
/// The later add subsumes the earlier guarded edit, which squash once refused to collapse.
#[tokio::test]
async fn stack_squash_collapses_a_twice_edited_line_and_folds_notes_without_coauthors() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    set_actor(&config, "Bart", "bart@old.invalid");
    ok(wheat(
        &workspace,
        &config,
        &[
            "change",
            "new",
            "-m",
            "Base file",
            "--path",
            "file",
            "--value",
            "l1\nl2\nl3\nl4\nl5\n",
        ],
    ));
    materialize(&workspace, &config);
    let created = ok(wheat(
        &workspace,
        &config,
        &["change", "new", "-m", "Edit line three\n\nWhy it changed."],
    ));
    let change = field(&created, "change ");
    fs::write(workspace.join("file"), "l1\nl2\nchanged\nl4\nl5\n").unwrap();
    ok(wheat(&workspace, &config, &["add", "file"]));
    fs::write(workspace.join("file"), "l1\nl2\nchanged again\nl4\nl5\n").unwrap();
    let latest = field(
        &ok(wheat(&workspace, &config, &["add", "file"])),
        "revision ",
    );
    let store = workspace_store(&workspace);
    assert!(
        !revision(&store, &latest)
            .await
            .transition
            .subsumed
            .is_empty(),
        "the second add must subsume the first edit for this test to mean anything"
    );

    set_actor(&config, "Bart", "bart@new.invalid");
    let result = json(
        &workspace,
        &config,
        &["--format", "json", "stack", "squash", "--no-coauthors"],
    );
    assert!(
        result["result"]["rewritten"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::String(change))
    );
    let replacement = result["result"]["revisions"][&latest].as_str().unwrap();
    let persisted = revision(&store, replacement).await;
    assert_eq!(persisted.predecessor, None);
    assert!(persisted.transition.subsumed.is_empty());
    assert_eq!(persisted.metadata.author.email, "bart@new.invalid");
    assert_eq!(
        persisted.metadata.message,
        "Edit line three\n\nWhy it changed."
    );
    assert_eq!(
        fs::read_to_string(workspace.join("file")).unwrap(),
        "l1\nl2\nchanged again\nl4\nl5\n"
    );
    ok(wheat(&workspace, &config, &["validate"]));
}

#[tokio::test]
async fn squash_persists_multi_actor_coauthors() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    set_actor(&config, "Alice", "alice@example.invalid");
    ok(wheat(
        &workspace,
        &config,
        &[
            "change", "new", "-m", "first", "--path", "file", "--value", "one",
        ],
    ));
    materialize(&workspace, &config);
    set_actor(&config, "Bob", "bob@example.invalid");
    fs::write(workspace.join("file"), "two").unwrap();
    let latest = ok(wheat(
        &workspace,
        &config,
        &["add", "--all", "-m", "second"],
    ));
    let old_latest = field(&latest, "revision ");
    set_actor(&config, "Squasher", "squasher@example.invalid");
    let result = json(
        &workspace,
        &config,
        &["--format", "json", "change", "squash"],
    );
    let replacement = result["result"]["revisions"][&old_latest].as_str().unwrap();
    let persisted = revision(&workspace_store(&workspace), replacement).await;
    assert_eq!(persisted.metadata.author.name, "Squasher");
    assert_eq!(persisted.metadata.committer.name, "Squasher");
    assert!(
        persisted
            .metadata
            .message
            .contains("Co-authored-by: Alice <alice@example.invalid>")
    );
    assert!(
        persisted
            .metadata
            .message
            .contains("Co-authored-by: Bob <bob@example.invalid>")
    );
}

#[test]
fn remote_exact_observed_envelope_sets_boundary_and_beats_generation() {
    let (_temp, workspace, config) = setup();
    let (_change, revisions, heads) = three_checkpoints(&workspace, &config);
    let store = workspace_store(&workspace);
    // A deliberately nonsensical generation proves selection is by the observed envelope.
    write_remote(
        &store,
        RepositoryHead {
            generation: heads[2].generation + 10_000,
            envelope: heads[0].envelope,
        },
    );
    let result = json(
        &workspace,
        &config,
        &["--format", "json", "change", "squash"],
    );
    let replacement = result["result"]["revisions"][&revisions[2]]
        .as_str()
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let persisted = runtime.block_on(revision(&store, replacement));
    assert_eq!(persisted.predecessor.unwrap().to_string(), revisions[0]);
}

#[test]
fn remote_latest_retained_is_noop_and_corrupt_observation_preserves_state() {
    let (_temp, workspace, config) = setup();
    let (_change, _revisions, heads) = three_checkpoints(&workspace, &config);
    let store = workspace_store(&workspace);
    write_remote(&store, heads[2]);
    let head_before = fs::read(store.join("repository.head")).unwrap();
    let workspace_before = fs::read(workspace.join(".wheat/workspace.json")).unwrap();
    let no_op = json(
        &workspace,
        &config,
        &["--format", "json", "change", "squash"],
    );
    assert_eq!(no_op["result"]["no_op"], true);
    assert_eq!(
        fs::read(store.join("repository.head")).unwrap(),
        head_before
    );
    assert_eq!(
        fs::read(workspace.join(".wheat/workspace.json")).unwrap(),
        workspace_before
    );
    assert!(!store.join("rewrite-transaction.json").exists());

    write_remote(
        &store,
        RepositoryHead {
            generation: heads[2].generation,
            envelope: wheat_core::RepositoryEnvelopeId::from_digest([0x5a; 32]),
        },
    );
    let refused = wheat(&workspace, &config, &["change", "squash"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("observed envelope"));
    assert_eq!(
        fs::read(store.join("repository.head")).unwrap(),
        head_before
    );
    assert_eq!(
        fs::read(workspace.join(".wheat/workspace.json")).unwrap(),
        workspace_before
    );
    assert!(!store.join("rewrite-transaction.json").exists());
}

#[test]
fn squash_handles_two_hundred_checkpoints_with_one_head_advance() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    ok(wheat(
        &workspace,
        &config,
        &[
            "change",
            "new",
            "-m",
            "checkpoint 0",
            "--path",
            "file",
            "--value",
            "0",
        ],
    ));
    materialize(&workspace, &config);
    for checkpoint in 1..200 {
        fs::write(workspace.join("file"), checkpoint.to_string()).unwrap();
        ok(wheat(
            &workspace,
            &config,
            &["add", "--all", "-m", &format!("checkpoint {checkpoint}")],
        ));
    }
    let store = workspace_store(&workspace);
    let before = head(&store);
    let report = ok(wheat(&workspace, &config, &["change", "squash"]));
    let after = head(&store);
    assert_eq!(after.generation, before.generation + 1);
    assert!(report.contains("revision "));
    assert!(report.contains("view "));
    assert!(report.contains("retained authorities:"));
    assert!(report.contains("consolidated conflicts: 0"));
    assert!(
        report.contains(
            "objects deleted: no; replaced revisions/views are GC candidates unless retained by an authority"
        )
    );
    ok(wheat(&workspace, &config, &["validate"]));
}

#[test]
fn change_squash_without_a_selection_lists_active_changes() {
    let (_temp, workspace, config) = setup();
    initialize(&workspace, &config);
    let first = ok(wheat(
        &workspace,
        &config,
        &[
            "change",
            "new",
            "-m",
            "first change",
            "--path",
            "a",
            "--value",
            "1",
        ],
    ));
    let first_change = field(&first, "change ");
    let second = ok(wheat(
        &workspace,
        &config,
        &[
            "change",
            "new",
            "-m",
            "second change",
            "--path",
            "b",
            "--value",
            "1",
        ],
    ));
    let second_change = field(&second, "change ");

    let metadata_path = workspace.join(".wheat/workspace.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata["selected_change"] = serde_json::Value::Null;
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let output = wheat(&workspace, &config, &["change", "squash"]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("active changes:"), "{error}");
    assert!(
        error.contains(&format!("{first_change}  first change")),
        "{error}"
    );
    assert!(
        error.contains(&format!("{second_change}  second change")),
        "{error}"
    );
}

#[test]
fn squash_updates_shared_workspace_metadata_without_changing_materialized_files() {
    let (temp, workspace, config) = setup();
    initialize(&workspace, &config);
    ok(wheat(
        &workspace,
        &config,
        &[
            "change", "new", "-m", "one", "--path", "file", "--value", "one",
        ],
    ));
    materialize(&workspace, &config);
    let second = temp.path().join("second");
    ok(wheat(
        &second,
        &config,
        &[
            "clone",
            workspace.to_str().unwrap(),
            second.to_str().unwrap(),
        ],
    ));
    let bytes = fs::read(second.join("file")).unwrap();
    fs::write(workspace.join("file"), "two").unwrap();
    ok(wheat(&workspace, &config, &["add", "--all", "-m", "two"]));
    let output = ok(wheat(
        &workspace,
        &config,
        &["--format", "json", "change", "squash"],
    ));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap()["result"]["workspaces"],
        2
    );
    assert_eq!(fs::read(second.join("file")).unwrap(), bytes);
    ok(wheat(&second, &config, &["status"]));
}
