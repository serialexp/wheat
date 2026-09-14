#![cfg(target_os = "linux")]

use std::{fs, path::Path, process::Command};

fn wheat(root: &Path, config: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .env("WHEAT_STORE_HOME", root.join("stores"))
        .args(arguments)
        .output()
        .unwrap()
}

fn json(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let output = wheat(root.path(), config.path(), &["--format=json", "init"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (key, value) in [
        ("actor.name", "Label Test"),
        ("actor.email", "labels@example.invalid"),
    ] {
        let output = wheat(root.path(), config.path(), &["config", "set", key, value]);
        assert!(output.status.success());
    }
    (root, config)
}

fn expected_head(document: &serde_json::Value) -> String {
    format!(
        "{}:{}",
        document["result"]["head"]["generation"].as_u64().unwrap(),
        document["result"]["head"]["envelope"].as_str().unwrap()
    )
}

fn workspace_store(root: &Path) -> String {
    let workspace: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".wheat/workspace.json")).unwrap()).unwrap();
    workspace["store"]["path"].as_str().unwrap().to_owned()
}

#[test]
fn labels_create_rename_delete_are_deterministic_and_reload() {
    let (root, config) = setup();
    for name in ["zebra", "alpha"] {
        let output = wheat(
            root.path(),
            config.path(),
            &["--format=json", "label", "create", name],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document = json(&output);
        assert_eq!(document["command"], "label.create");
        assert_eq!(document["result"]["schema_version"], 1);
    }

    let listed = wheat(root.path(), config.path(), &["--format=json", "labels"]);
    assert!(listed.status.success());
    let listed = json(&listed);
    assert_eq!(listed["command"], "labels");
    assert_eq!(listed["result"]["schema_version"], 1);
    assert_eq!(listed["result"]["labels"][0]["name"], "alpha");
    assert_eq!(listed["result"]["labels"][1]["name"], "main");
    assert_eq!(listed["result"]["labels"][2]["name"], "zebra");
    assert!(listed["result"]["labels"][0]["view"].is_string());

    let renamed = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "rename", "alpha", "middle"],
    );
    assert!(renamed.status.success());
    let deleted = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "delete", "zebra"],
    );
    assert!(deleted.status.success());

    let reloaded = wheat(root.path(), config.path(), &["--format=json", "labels"]);
    let reloaded = json(&reloaded);
    assert_eq!(reloaded["result"]["labels"].as_array().unwrap().len(), 2);
    assert_eq!(reloaded["result"]["labels"][0]["name"], "main");
    assert_eq!(reloaded["result"]["labels"][1]["name"], "middle");
}

#[test]
fn label_mutations_reject_duplicates_missing_names_and_stale_full_head() {
    let (root, config) = setup();
    let before = json(&wheat(
        root.path(),
        config.path(),
        &["--format=json", "labels"],
    ));
    let stale = expected_head(&before);

    let created = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "create", "one"],
    );
    assert!(created.status.success());

    let duplicate = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "create", "one"],
    );
    assert_eq!(duplicate.status.code(), Some(2));
    assert_eq!(json(&duplicate)["error"]["code"], "invalid_input");

    let missing = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "rename", "missing", "other"],
    );
    assert_eq!(missing.status.code(), Some(2));

    let stale_attempt = wheat(
        root.path(),
        config.path(),
        &[
            "--format=json",
            "label",
            "create",
            "two",
            "--expected-head",
            &stale,
        ],
    );
    assert_eq!(stale_attempt.status.code(), Some(3));
    assert_eq!(json(&stale_attempt)["error"]["code"], "stale_state");

    let listed = json(&wheat(
        root.path(),
        config.path(),
        &["--format=json", "labels"],
    ));
    assert_eq!(listed["result"]["labels"].as_array().unwrap().len(), 2);
    assert_eq!(listed["result"]["labels"][0]["name"], "main");
    assert_eq!(listed["result"]["labels"][1]["name"], "one");
}

#[test]
fn create_defaults_to_the_workspace_effective_view() {
    let (root, config) = setup();
    for (key, value) in [
        ("actor.name", "Label Tester"),
        ("actor.email", "labels@example.invalid"),
    ] {
        let configured = wheat(root.path(), config.path(), &["config", "set", key, value]);
        assert!(configured.status.success());
    }
    let before = json(&wheat(
        root.path(),
        config.path(),
        &["--format=json", "labels"],
    ));
    let initial = before["result"]["labels"][0]["view"].as_str().unwrap();
    let changed = wheat(
        root.path(),
        config.path(),
        &["change", "new", "-m", "advance working view"],
    );
    assert!(changed.status.success());
    let materialized = wheat(root.path(), config.path(), &["materialize"]);
    assert!(materialized.status.success());
    let created = json(&wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "create", "from-effective"],
    ));
    let selected = created["result"]["label"]["view"].as_str().unwrap();
    assert_ne!(selected, initial);
}

#[test]
fn explicit_store_label_operations_do_not_require_a_workspace() {
    let (root, config) = setup();
    let store = workspace_store(root.path());
    let outside = tempfile::tempdir().unwrap();
    let current = json(&wheat(
        outside.path(),
        config.path(),
        &["--store", &store, "--format=json", "labels"],
    ));
    let view = current["result"]["labels"][0]["view"].as_str().unwrap();
    let created = wheat(
        outside.path(),
        config.path(),
        &[
            "--store", &store, "label", "create", "detached", "--view", view,
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let renamed = wheat(
        outside.path(),
        config.path(),
        &["--store", &store, "label", "rename", "detached", "remote"],
    );
    assert!(renamed.status.success());
    let listed = json(&wheat(
        outside.path(),
        config.path(),
        &["--store", &store, "--format=json", "labels"],
    ));
    assert_eq!(listed["result"]["labels"][1]["name"], "remote");
    let deleted = wheat(
        outside.path(),
        config.path(),
        &["--store", &store, "label", "delete", "remote"],
    );
    assert!(deleted.status.success());

    let missing_view = wheat(
        outside.path(),
        config.path(),
        &[
            "--store",
            &store,
            "--format=json",
            "label",
            "create",
            "implicit",
        ],
    );
    assert_eq!(missing_view.status.code(), Some(2));
    assert_eq!(json(&missing_view)["error"]["code"], "invalid_input");
}

#[test]
fn explicit_store_ignores_a_workspace_linked_to_another_store() {
    let (first, first_config) = setup();
    assert!(
        wheat(
            first.path(),
            first_config.path(),
            &["label", "create", "shared"]
        )
        .status
        .success()
    );
    assert!(
        wheat(first.path(), first_config.path(), &["switch", "shared"],)
            .status
            .success()
    );

    let second = tempfile::tempdir().unwrap();
    let second_config = tempfile::tempdir().unwrap();
    assert!(
        wheat(
            second.path(),
            second_config.path(),
            &["--format=json", "init"],
        )
        .status
        .success()
    );
    let second_store = workspace_store(second.path());
    let second_labels = json(&wheat(
        second.path(),
        second_config.path(),
        &["--format=json", "labels"],
    ));
    let second_view = second_labels["result"]["labels"][0]["view"]
        .as_str()
        .unwrap();
    assert!(
        wheat(
            second.path(),
            second_config.path(),
            &["label", "create", "shared", "--view", second_view],
        )
        .status
        .success()
    );

    let deleted = wheat(
        first.path(),
        first_config.path(),
        &["--store", &second_store, "label", "delete", "shared"],
    );
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
}

#[test]
fn delete_rejects_the_workspace_selected_label() {
    let (root, config) = setup();
    let created = wheat(root.path(), config.path(), &["label", "create", "selected"]);
    assert!(created.status.success());
    let materialized = wheat(root.path(), config.path(), &["switch", "selected"]);
    assert!(materialized.status.success());

    let deleted = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "delete", "selected"],
    );
    assert_eq!(deleted.status.code(), Some(2));
    assert_eq!(json(&deleted)["error"]["code"], "invalid_input");

    let renamed = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "rename", "selected", "renamed"],
    );
    assert_eq!(renamed.status.code(), Some(2));
    assert_eq!(json(&renamed)["error"]["code"], "invalid_input");
}

#[test]
fn labels_enforce_the_repository_size_bound_on_create() {
    use wheat_core::{RepositoryEnvelopeRecord, ViewLabelManifestRecord};
    use wheat_store::{
        ImmutableObjectStore, LocalPublicationStore, LooseObjectStore, PublicationStore,
        RepositoryReader,
    };

    let (root, config) = setup();
    let workspace: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".wheat/workspace.json")).unwrap())
            .unwrap();
    let store = Path::new(workspace["store"]["path"].as_str().unwrap());
    let heads = LocalPublicationStore::new(store.join("repository.head"));
    let objects = LooseObjectStore::new(store.join("objects"));
    let head = heads.read().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let content = wheat_store::ContentStore::new(store.join("content"));
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = runtime.block_on(reader.envelope(head.envelope)).unwrap();
    let current = runtime
        .block_on(reader.view_label_manifest(envelope.labels))
        .unwrap();
    let view = current.labels["main"].view;
    let labels =
        ViewLabelManifestRecord::new((0..10_000).map(|index| (format!("label-{index:05}"), view)))
            .unwrap();
    runtime
        .block_on(objects.put_if_absent(labels.id().into(), &labels.canonical_bytes()))
        .unwrap();
    let oversized_envelope = RepositoryEnvelopeRecord {
        state: envelope.state,
        labels: labels.id(),
    };
    runtime
        .block_on(objects.put_if_absent(
            oversized_envelope.id().into(),
            &oversized_envelope.canonical_bytes(),
        ))
        .unwrap();
    let oversized_head = head.next(oversized_envelope.id()).unwrap();
    heads.compare_and_advance(head, oversized_head).unwrap();

    let rejected = wheat(
        root.path(),
        config.path(),
        &["--format=json", "label", "create", "one-too-many"],
    );
    assert_eq!(rejected.status.code(), Some(5));
    assert_eq!(json(&rejected)["error"]["code"], "limit_exceeded");
}

#[test]
fn label_command_parser_requires_structural_subcommands_and_full_head() {
    let (root, config) = setup();
    let missing = wheat(root.path(), config.path(), &["--format=json", "label"]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["error"]["code"], "invalid_arguments");

    let malformed = wheat(
        root.path(),
        config.path(),
        &[
            "--format=json",
            "label",
            "delete",
            "name",
            "--expected-head",
            "7",
        ],
    );
    assert_eq!(malformed.status.code(), Some(2));
    assert_eq!(json(&malformed)["error"]["code"], "invalid_arguments");

    let empty_switch = wheat(root.path(), config.path(), &["--format=json", "switch", ""]);
    assert_eq!(empty_switch.status.code(), Some(2));
    assert_eq!(json(&empty_switch)["error"]["code"], "invalid_input");

    let obsolete_source = wheat(
        root.path(),
        config.path(),
        &["--format=json", "materialize", "--source", "label:main"],
    );
    assert_eq!(obsolete_source.status.code(), Some(2));
    assert_eq!(json(&obsolete_source)["error"]["code"], "invalid_arguments");
}
