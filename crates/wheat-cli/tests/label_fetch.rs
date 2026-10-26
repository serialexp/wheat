#![cfg(target_os = "linux")]

//! A fetch takes the remote's label moves and deletions, unless they would hide local
//! work: a frontier of the label's local view that the new view does not cover and no
//! workspace shows.

mod remote_support;

use remote_support::sides::sides;

#[test]
fn a_remote_move_that_hides_an_unshown_frontier_is_refused() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let base = sides.reader.label_view(config, "main").unwrap();
    // Local work the reader's workspace then leaves for the label's view.
    let local = sides.reader.change(config, "local.txt");
    sides
        .reader
        .ok_json(config, &["switch", "main", "--frontier", &base]);
    let remote = sides.writer.change(config, "remote.txt");
    sides.writer.ok_json(config, &["label", "move", "main"]);
    sides.writer.ok_json(config, &["publish"]);

    let refused = sides.reader.fails(config, &["fetch"]);
    assert!(refused.contains("label main"), "{refused}");
    assert!(refused.contains(&local), "{refused}");
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), base);

    // Shown by the workspace, the work stays reachable: the fetch takes the move, and
    // refresh joins the work with the label's new view.
    sides
        .reader
        .ok_json(config, &["switch", "main", "--frontier", &local]);
    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), remote);
    let refreshed = sides.reader.ok_json(config, &["refresh"]);
    assert_eq!(refreshed["integrated"], true, "{refreshed}");
    for path in ["local.txt", "remote.txt"] {
        assert_eq!(
            std::fs::read_to_string(sides.reader.workspace.join(path)).unwrap(),
            path
        );
    }
}

#[test]
fn a_remote_move_that_covers_the_frontiers_is_taken() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let remote = sides.writer.change(config, "remote.txt");
    sides.writer.ok_json(config, &["label", "move", "main"]);
    sides.writer.ok_json(config, &["publish"]);

    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), remote);
    sides.reader.ok_json(config, &["refresh"]);
    assert_eq!(
        std::fs::read_to_string(sides.reader.workspace.join("remote.txt")).unwrap(),
        "remote.txt"
    );
}

#[test]
fn a_remote_deletion_that_hides_an_unshown_frontier_is_refused() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let base = sides.reader.label_view(config, "main").unwrap();
    sides
        .writer
        .ok_json(config, &["label", "create", "feature"]);
    sides.writer.ok_json(config, &["publish"]);
    sides.reader.ok_json(config, &["fetch"]);
    sides.reader.ok_json(config, &["switch", "feature"]);
    let work = sides.reader.change(config, "feature.txt");
    // The work is also a frontier of main; the workspace leaves it for main's view.
    sides
        .reader
        .ok_json(config, &["switch", "main", "--frontier", &base]);
    sides
        .writer
        .ok_json(config, &["label", "delete", "feature"]);
    sides.writer.ok_json(config, &["publish"]);

    let refused = sides.reader.fails(config, &["fetch"]);
    assert!(refused.contains("label feature"), "{refused}");
    assert!(refused.contains(&work), "{refused}");
    assert!(sides.reader.label_view(config, "feature").is_some());

    // A deleted label's own view needs no workspace: without local work on it, the
    // deletion is taken.
    let (sides, _server) = self::sides();
    let config = &sides.config;
    sides
        .writer
        .ok_json(config, &["label", "create", "feature"]);
    sides.writer.ok_json(config, &["publish"]);
    sides.reader.ok_json(config, &["fetch"]);
    sides
        .writer
        .ok_json(config, &["label", "delete", "feature"]);
    sides.writer.ok_json(config, &["publish"]);
    sides.reader.ok_json(config, &["fetch"]);
    assert!(sides.reader.label_view(config, "feature").is_none());
}
