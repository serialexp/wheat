#![cfg(target_os = "linux")]

//! Hidden labels, named with a leading dot, travel like any other label: a clone and
//! a fetch take them, and a publish keeps them as they are. `wheat labels` leaves them
//! out unless `--hidden` is given or the remote binding shows them by a glob pattern
//! (`wheat clone --label`, `wheat remote labels`).

use crate::remote_support;

use remote_support::{
    ok,
    sides::{Side, sides},
};
use wheat_client::{remote_config::RemoteConfig, workspace_metadata::WorkspaceMetadata};

fn names(listing: &serde_json::Value) -> Vec<String> {
    listing["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|label| label["name"].as_str().unwrap().to_owned())
        .collect()
}

/// The view each label names, hidden ones too.
fn views(side: &Side, config: &std::path::Path) -> Vec<(String, String)> {
    side.ok_json(config, &["labels", "--hidden"])["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|label| {
            (
                label["name"].as_str().unwrap().to_owned(),
                label["view"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn clone(server: &str, config: &std::path::Path, extra: &[&str]) -> (tempfile::TempDir, Side) {
    let root = tempfile::tempdir().unwrap();
    let (workspace, stores) = (root.path().join("clone"), root.path().join("stores"));
    let mut args = vec!["clone", server, workspace.to_str().unwrap()];
    args.extend_from_slice(extra);
    ok(root.path(), &stores, config, &args);
    (root, Side { workspace, stores })
}

#[test]
fn hidden_labels_are_kept_but_listed_only_when_asked() {
    let (sides, server) = sides();
    let config = &sides.config;
    sides.writer.change(config, "main.txt");
    for name in [".gothab/pr/1", ".gothab/pr/1/merge", ".gothab/other"] {
        sides.writer.ok_json(config, &["label", "create", name]);
    }
    sides.writer.ok_json(config, &["publish"]);

    let (_root, third) = clone(&server.url, config, &[]);
    // Hidden labels sort first, but a clone never starts on one.
    assert_eq!(
        WorkspaceMetadata::read(&third.workspace)
            .unwrap()
            .active_label
            .as_deref(),
        Some("main")
    );
    let listing = third.ok_json(config, &["labels"]);
    assert_eq!(names(&listing), ["main"]);
    assert_eq!(listing["hidden_not_listed"], 3, "{listing}");
    assert_eq!(
        names(&third.ok_json(config, &["labels", "--hidden"])),
        [
            ".gothab/other",
            ".gothab/pr/1",
            ".gothab/pr/1/merge",
            "main"
        ]
    );
    // Named directly, a hidden label works like any other.
    third.ok_json(config, &["switch", ".gothab/pr/1"]);
    third.ok_json(config, &["switch", "main"]);

    // The binding shows what its patterns match: `*` within one component.
    let shown = third.ok_json(config, &["remote", "labels", "--add", ".gothab/pr/*"]);
    assert_eq!(
        shown["shown_hidden_labels"],
        serde_json::json!([".gothab/pr/*"])
    );
    let listing = third.ok_json(config, &["labels"]);
    assert_eq!(names(&listing), [".gothab/pr/1", "main"]);
    assert_eq!(listing["hidden_not_listed"], 2);
    let refused = third.fails(config, &["remote", "labels", "--remove", ".nothing"]);
    assert!(refused.contains("no hidden labels by"), "{refused}");
    third.ok_json(config, &["remote", "labels", "--remove", ".gothab/pr/*"]);
    assert_eq!(names(&third.ok_json(config, &["labels"])), ["main"]);

    // Work published from the clone leaves the hidden labels where they were.
    let before = views(&sides.writer, config);
    third.change(config, "third.txt");
    third.ok_json(config, &["publish"]);
    sides.writer.ok_json(config, &["pull"]);
    let after = views(&sides.writer, config);
    assert_eq!(
        before
            .iter()
            .filter(|(name, _)| name.starts_with('.'))
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|(name, _)| name.starts_with('.'))
            .collect::<Vec<_>>(),
    );
    assert_ne!(before, after, "main moved");
}

#[test]
fn a_clone_keeps_the_patterns_it_is_given() {
    let (sides, server) = sides();
    let config = &sides.config;
    sides
        .writer
        .ok_json(config, &["label", "create", ".review/a"]);
    sides.writer.ok_json(config, &["label", "create", ".other"]);
    sides.writer.ok_json(config, &["publish"]);

    let (_root, third) = clone(
        &server.url,
        config,
        &["--label", ".review/**", "--label", ".review/**"],
    );
    assert_eq!(
        names(&third.ok_json(config, &["labels"])),
        [".review/a", "main"]
    );
    let store = WorkspaceMetadata::read(&third.workspace)
        .unwrap()
        .store
        .path;
    let binding = RemoteConfig::read(&store).unwrap().unwrap();
    assert_eq!(binding.shown_hidden_labels, [".review/**"]);

    let refused = remote_support::fails(
        _root.path(),
        &third.stores,
        config,
        &["clone", &server.url, "again", "--label", "["],
    );
    assert!(refused.contains("invalid label pattern"), "{refused}");
}
