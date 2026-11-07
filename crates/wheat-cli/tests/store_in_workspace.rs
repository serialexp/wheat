#![cfg(target_os = "linux")]

//! `--store-in-workspace`: a workspace that keeps its store in `.wheat/store`, so the
//! workspace directory holds everything, as a CI job container that mounts only its
//! checkout needs.

use std::path::Path;

use crate::remote_support;

use remote_support::{fails, ok, ok_json, sides::sides, write_config};

/// The stores under the store home `stores`, which may not exist.
fn stores_under(stores: &Path) -> usize {
    std::fs::read_dir(stores).map_or(0, |entries| entries.count())
}

#[test]
fn a_clone_keeps_its_store_in_the_workspace() {
    let (sides, server) = sides();
    let config = &sides.config;
    let first = sides.writer.change(config, "one.txt");
    sides.writer.ok_json(config, &["publish"]);

    let root = tempfile::tempdir().unwrap();
    let (workspace, stores) = (root.path().join("ci"), root.path().join("ci-stores"));
    ok(
        root.path(),
        &stores,
        config,
        &[
            "clone",
            &server.url,
            workspace.to_str().unwrap(),
            "--view",
            &first,
            "--store-in-workspace",
        ],
    );
    assert!(workspace.join(".wheat/store/store.meta").is_file());
    assert_eq!(stores_under(&stores), 0, "nothing goes to the store home");
    let status = ok_json(&workspace, &stores, config, &["status"]);
    assert_eq!(status["effective_view"], first.as_str(), "{status}");
    // `.wheat` is one ignored entry, as in every workspace; nothing in the store is
    // looked at.
    assert_eq!(
        status["untracked"],
        serde_json::json!([{ "ignored": true, "is_directory": true, "path": ".wheat" }]),
        "{status}"
    );
    assert_eq!(status["changes"], serde_json::json!([]), "{status}");

    // It fetches into its own store and follows a label again.
    let second = sides.writer.change(config, "two.txt");
    sides.writer.ok_json(config, &["publish"]);
    ok(&workspace, &stores, config, &["fetch"]);
    let switched = ok_json(&workspace, &stores, config, &["switch", "main"]);
    assert_eq!(switched["label"], "main", "{switched}");
    let status = ok_json(&workspace, &stores, config, &["status"]);
    assert_eq!(status["effective_view"], second.as_str(), "{status}");
    assert!(workspace.join("two.txt").is_file());
    assert_eq!(stores_under(&stores), 0);

    // Unregistering would delete the store with the control directory.
    let refused = fails(&workspace, &stores, config, &["workspace", "unregister"]);
    assert!(refused.contains("keeps its own store"), "{refused}");
    assert!(workspace.join(".wheat/store/store.meta").is_file());
}

#[test]
fn a_failed_clone_into_the_workspace_leaves_it_empty_for_a_retry() {
    let (sides, server) = sides();
    let config = &sides.config;
    let first = sides.writer.change(config, "one.txt");
    sides.writer.ok_json(config, &["publish"]);
    // A well-formed view the remote doesn't have: the clone fails after its fetch.
    let last = if first.ends_with('0') { "1" } else { "0" };
    let missing = format!("{}{last}", &first[..first.len() - 1]);

    let root = tempfile::tempdir().unwrap();
    let (workspace, stores) = (root.path().join("ci"), root.path().join("ci-stores"));
    let clone = |view: &str| {
        vec![
            "clone".to_owned(),
            server.url.clone(),
            workspace.to_str().unwrap().to_owned(),
            "--view".to_owned(),
            view.to_owned(),
            "--store-in-workspace".to_owned(),
        ]
    };
    let args = clone(&missing);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let failed = fails(root.path(), &stores, config, &args);
    assert!(failed.contains("is not in this repository"), "{failed}");
    assert!(!workspace.join(".wheat").exists(), "the store went with it");

    let args = clone(&first);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ok(root.path(), &stores, config, &args);
    assert!(workspace.join("one.txt").is_file());
    assert_eq!(stores_under(&stores), 0);
}

#[test]
fn init_keeps_its_store_in_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let (workspace, stores) = (root.path().join("new"), root.path().join("stores"));
    ok(
        root.path(),
        &stores,
        &config,
        &["init", workspace.to_str().unwrap(), "--store-in-workspace"],
    );
    assert!(workspace.join(".wheat/store/store.meta").is_file());
    assert_eq!(stores_under(&stores), 0);
    std::fs::write(workspace.join("file.txt"), "hello\n").unwrap();
    let status = ok_json(&workspace, &stores, &config, &["status"]);
    let untracked: Vec<_> = status["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["path"].as_str().unwrap(), entry["ignored"] == true))
        .collect();
    assert_eq!(
        untracked,
        [(".wheat", true), ("file.txt", false)],
        "{status}"
    );
}

#[test]
fn store_in_workspace_refuses_a_given_store_and_a_local_source() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let stores = root.path().join("stores");
    let source = root.path().join("source");
    ok(
        root.path(),
        &stores,
        &config,
        &["init", source.to_str().unwrap()],
    );

    let target = root.path().join("target");
    let refused = fails(
        root.path(),
        &stores,
        &config,
        &[
            "clone",
            source.to_str().unwrap(),
            target.to_str().unwrap(),
            "--store-in-workspace",
        ],
    );
    assert!(refused.contains("needs a native remote"), "{refused}");
    let other = root.path().join("other-store");
    let refused = fails(
        root.path(),
        &stores,
        &config,
        &[
            "--store",
            other.to_str().unwrap(),
            "init",
            target.to_str().unwrap(),
            "--store-in-workspace",
        ],
    );
    assert!(refused.contains("takes no --store"), "{refused}");
    assert!(!target.exists());
}
