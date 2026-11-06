#![cfg(target_os = "linux")]

//! A clone reads the remote's labels from the head it fetched, and becomes active on
//! main when the remote has one.

use crate::remote_support;

use remote_support::{ok, sides::sides};
use wheat_client::workspace_metadata::WorkspaceMetadata;

#[test]
fn a_clone_is_active_on_main_when_other_labels_sort_first() {
    let (sides, server) = sides();
    let config = &sides.config;
    sides.writer.change(config, "main.txt");
    sides
        .writer
        .ok_json(config, &["label", "create", "a-feature"]);
    sides.writer.ok_json(config, &["publish"]);

    let root = tempfile::tempdir().unwrap();
    let (workspace, stores) = (root.path().join("third"), root.path().join("stores"));
    ok(
        root.path(),
        &stores,
        config,
        &["clone", &server.url, workspace.to_str().unwrap()],
    );
    let metadata = WorkspaceMetadata::read(&workspace).unwrap();
    assert_eq!(metadata.active_label.as_deref(), Some("main"));
    let third = remote_support::sides::Side { workspace, stores };
    let labels = third.ok_json(config, &["labels"])["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|label| label["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["a-feature", "main"]);
}
