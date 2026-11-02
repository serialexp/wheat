#![cfg(target_os = "linux")]

//! `wheat publish` moves the active label to the workspace's view before it sends
//! anything, under the checks `label move` makes, and the server takes a label only
//! forward unless the publish forces labels.

use crate::remote_support;

use remote_support::sides::{Side, sides};
use wheat_client::workspace_metadata::WorkspaceMetadata;

fn read(side: &Side, path: &str) -> String {
    std::fs::read_to_string(side.workspace.join(path)).unwrap()
}

#[test]
fn publish_moves_the_label_and_the_remote_sees_it() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let base = sides.writer.label_view(config, "main").unwrap();
    let work = sides.writer.change(config, "work.txt");

    let published = sides.writer.ok_json(config, &["publish"]);
    assert_eq!(
        published["label_moved"]["from"],
        base.as_str(),
        "{published}"
    );
    assert_eq!(
        published["label_moved"]["label"]["view"],
        work.as_str(),
        "{published}"
    );
    assert_eq!(sides.writer.label_view(config, "main").unwrap(), work);
    // The workspace follows the label at its new view.
    let metadata = WorkspaceMetadata::read(&sides.writer.workspace).unwrap();
    assert_eq!(metadata.basis.to_string(), work);
    assert_eq!(metadata.selected_frontier, None);

    sides.reader.ok_json(config, &["pull"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), work);
    assert_eq!(read(&sides.reader, "work.txt"), "work.txt");

    // Nothing left to move: the next publish moves nothing.
    let again = sides.writer.ok_json(config, &["publish"]);
    assert!(again["label_moved"].is_null(), "{again}");
}

/// A publish that would leave a frontier of the label behind refuses before it sends
/// anything; `--keep-label` publishes the work without moving the label, and
/// `--force-label` moves it anyway.
#[test]
fn publish_refuses_to_leave_a_frontier_behind() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let base = sides.writer.label_view(config, "main").unwrap();
    let left = sides.writer.change(config, "left.txt");
    sides
        .writer
        .ok_json(config, &["switch", "main", "--frontier", &base]);
    let kept = sides.writer.change(config, "kept.txt");

    let refused = sides.writer.fails(config, &["publish"]);
    assert!(refused.contains(&left), "{refused}");
    assert!(refused.contains("--keep-label"), "{refused}");
    assert!(refused.contains("--force-label"), "{refused}");
    assert_eq!(sides.writer.label_view(config, "main").unwrap(), base);
    sides.reader.ok_json(config, &["fetch"]);
    let frontiers = sides.reader.ok_json(config, &["frontier", "list"]);
    assert_eq!(
        frontiers["frontiers"],
        serde_json::json!([]),
        "nothing was sent"
    );

    let published = sides.writer.ok_json(config, &["publish", "--keep-label"]);
    assert!(published["label_moved"].is_null(), "{published}");
    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), base);

    let forced = sides.writer.ok_json(config, &["publish", "--force-label"]);
    assert_eq!(
        forced["label_moved"]["label"]["view"],
        kept.as_str(),
        "{forced}"
    );
    // The reader holds both frontiers now, so the fetch rule refuses the move that
    // leaves one behind there too, until the reader takes the remote's labels.
    let hidden = sides.reader.fails(config, &["fetch"]);
    assert!(hidden.contains(&left), "{hidden}");
    sides.reader.ok_json(config, &["fetch", "--force"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), kept);
}

/// The server refuses a label moved backward unless the publish forces labels.
#[test]
fn the_server_refuses_a_backward_label_unless_forced() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let base = sides.writer.label_view(config, "main").unwrap();
    let work = sides.writer.change(config, "work.txt");
    sides.writer.ok_json(config, &["publish"]);

    // Back to the base view locally, which lacks the published change.
    sides.writer.ok_json(
        config,
        &["label", "move", "main", "--view", &base, "--force"],
    );
    sides
        .writer
        .ok_json(config, &["switch", "main", "--frontier", &base]);
    let refused = sides.writer.fails(config, &["publish", "--keep-label"]);
    assert!(refused.contains("label main would move"), "{refused}");
    assert!(refused.contains("--force-label"), "{refused}");
    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), work);

    sides.writer.ok_json(config, &["publish", "--force-label"]);
    sides.reader.ok_json(config, &["fetch", "--force"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), base);
}

/// A publish the server refuses keeps the label moved here; the next pull reconciles
/// it with the remote's.
#[test]
fn a_refused_publish_keeps_the_local_move() {
    let (sides, _server) = sides();
    let config = &sides.config;
    sides.reader.change(config, "first.txt");
    sides.reader.ok_json(config, &["publish"]);
    let mine = sides.writer.change(config, "mine.txt");

    let refused = sides.writer.fails(config, &["publish"]);
    assert!(refused.contains("stays moved"), "{refused}");
    assert_eq!(sides.writer.label_view(config, "main").unwrap(), mine);
}

/// Alice and Bob on `main`: Bob integrates his work (the label moves to it), Alice
/// publishes hers first, so Bob's publish is refused; Bob pulls, which joins the two
/// and moves the label to the result, and publishes; Alice pulls and has both.
#[test]
fn two_writers_moving_the_label_meet_through_pull() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let (alice, bob) = (&sides.writer, &sides.reader);

    bob.change(config, "bob.txt");
    let integrated = bob.ok_json(config, &["integrate"]);
    assert!(!integrated["label_moved"].is_null(), "{integrated}");
    alice.change(config, "alice.txt");
    alice.ok_json(config, &["publish"]);

    let refused = bob.fails(config, &["publish"]);
    assert!(refused.contains("publish rejected"), "{refused}");

    let pulled = bob.ok_json(config, &["pull"]);
    assert!(!pulled["integrate"]["label_moved"].is_null(), "{pulled}");
    for path in ["alice.txt", "bob.txt"] {
        assert_eq!(read(bob, path), path);
    }
    bob.ok_json(config, &["publish"]);

    alice.ok_json(config, &["pull"]);
    for path in ["alice.txt", "bob.txt"] {
        assert_eq!(read(alice, path), path);
    }
    assert_eq!(
        alice.label_view(config, "main"),
        bob.label_view(config, "main")
    );
}
