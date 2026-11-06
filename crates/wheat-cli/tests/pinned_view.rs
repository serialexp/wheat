#![cfg(target_os = "linux")]

//! A workspace made for one exact view, as a CI runner checks out the view that
//! triggered it after its label moved on: it shows that view and follows no label,
//! so the commands that read a label refuse until it switches to one.

use crate::remote_support;

use remote_support::{ok, sides::sides};

#[test]
fn a_clone_of_one_view_shows_it_and_follows_no_label() {
    let (sides, server) = sides();
    let config = &sides.config;
    let first = sides.writer.change(config, "one.txt");
    sides.writer.ok_json(config, &["publish"]);
    let second = sides.writer.change(config, "two.txt");
    sides.writer.ok_json(config, &["publish"]);
    assert_eq!(
        sides.writer.label_view(config, "main").as_deref(),
        Some(second.as_str())
    );

    // A fresh runner clones the first view, though main has moved past it.
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
        ],
    );
    let ci = remote_support::sides::Side { workspace, stores };
    let shows = |view: &str, files: &[(&str, bool)]| {
        let status = ci.ok_json(config, &["status"]);
        assert_eq!(status["effective_view"], view, "{status}");
        for (file, present) in files {
            assert_eq!(ci.workspace.join(file).exists(), *present, "{file}");
        }
        status
    };
    let status = shows(&first, &[("one.txt", true), ("two.txt", false)]);
    assert!(status["active_label"].is_null(), "{status}");
    assert_eq!(status["label_binding_drifted"], false, "{status}");
    assert!(status["current_label_binding"].is_null(), "{status}");

    for command in [
        &["refresh"][..],
        &["integrate"],
        &["pull"],
        &["frontier", "list"],
        &["publish", "--force-label"],
    ] {
        let refused = ci.fails(config, command);
        assert!(refused.contains("follows none"), "{command:?}: {refused}");
        assert!(
            refused.contains("wheat switch <label>"),
            "{command:?}: {refused}"
        );
    }
    // A label named outright still lists.
    let listed = ci.ok_json(config, &["frontier", "list", "--label", "main"]);
    assert_eq!(listed["basis"], second.as_str(), "{listed}");

    // Switching to the label follows it again; switching to the view leaves it.
    let switched = ci.ok_json(config, &["switch", "main"]);
    assert_eq!(switched["label"], "main", "{switched}");
    let status = shows(&second, &[("one.txt", true), ("two.txt", true)]);
    assert_eq!(status["active_label"], "main", "{status}");
    let switched = ci.ok_json(config, &["switch", "--view", &first]);
    assert!(switched["label"].is_null(), "{switched}");
    let status = shows(&first, &[("one.txt", true), ("two.txt", false)]);
    assert!(status["active_label"].is_null(), "{status}");

    // A view this repository does not have is refused, and the workspace stays.
    let unknown = "ab".repeat(32);
    let refused = ci.fails(config, &["switch", "--view", &unknown]);
    assert!(refused.contains("is not in this repository"), "{refused}");
    shows(&first, &[("one.txt", true), ("two.txt", false)]);
    // A label and a view at once is a usage error.
    let refused = ci.fails(config, &["switch", "main", "--view", &first]);
    assert!(refused.contains("cannot be used with"), "{refused}");

    // Work made here publishes without moving a label.
    let third = ci.change(config, "three.txt");
    let published = ci.ok_json(config, &["publish"]);
    assert!(published["label"].is_null(), "{published}");
    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(
        sides.reader.label_view(config, "main").as_deref(),
        Some(second.as_str())
    );
    // The work arrived: another clone can show its view.
    sides.reader.ok_json(config, &["switch", "--view", &third]);
    assert!(sides.reader.workspace.join("three.txt").exists());
    assert!(!sides.reader.workspace.join("two.txt").exists());
}
