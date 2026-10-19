#![cfg(target_os = "linux")]

//! `wheat integrate` joins the active label's frontiers into one view, such as the
//! local and the remote work a fetch kept side by side.

mod remote_support;

use std::fs;
use std::path::{Path, PathBuf};

use remote_support::{fails, ok, ok_json, serve, write_config};

struct Sides {
    _root: tempfile::TempDir,
    config: PathBuf,
    writer: (PathBuf, PathBuf),
    reader: (PathBuf, PathBuf),
}

/// Two clones of one remote.
fn sides() -> (Sides, remote_support::Server) {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let server = serve(root.path(), &config);
    let clone = |name: &str| {
        let stores = root.path().join(format!("{name}-stores"));
        let workspace = root.path().join(name);
        ok(
            root.path(),
            &stores,
            &config,
            &["clone", &server.url, workspace.to_str().unwrap()],
        );
        (workspace, stores)
    };
    let writer = clone("writer");
    let reader = clone("reader");
    (
        Sides {
            _root: root,
            config,
            writer,
            reader,
        },
        server,
    )
}

/// A change writing `value` to `path`.
fn change(side: &(PathBuf, PathBuf), config: &Path, path: &str, value: &str) {
    ok(
        &side.0,
        &side.1,
        config,
        &[
            "change",
            "new",
            "--path",
            path,
            "--value",
            value,
            "--message",
            path,
        ],
    );
}

fn frontier_count(side: &(PathBuf, PathBuf), config: &Path) -> usize {
    ok_json(&side.0, &side.1, config, &["frontier", "list"])["frontiers"]
        .as_array()
        .unwrap()
        .len()
}

/// Local and remote work each on its own frontier after a fetch.
fn diverged(writes: [(&str, &str); 2]) -> (Sides, remote_support::Server) {
    let (sides, server) = sides();
    change(&sides.writer, &sides.config, writes[0].0, writes[0].1);
    ok(
        &sides.writer.0,
        &sides.writer.1,
        &sides.config,
        &["publish"],
    );
    change(&sides.reader, &sides.config, writes[1].0, writes[1].1);
    ok(&sides.reader.0, &sides.reader.1, &sides.config, &["fetch"]);
    assert_eq!(frontier_count(&sides.reader, &sides.config), 2);
    (sides, server)
}

#[test]
fn integrate_joins_the_fetched_work_with_the_local_work() {
    let (sides, _server) = diverged([("remote.txt", "remote"), ("local.txt", "local")]);
    let (reader, stores) = &sides.reader;

    let joined = ok_json(reader, stores, &sides.config, &["integrate"]);

    assert_eq!(joined["integrated"], true, "{joined}");
    assert_eq!(joined["composed"].as_array().unwrap().len(), 2, "{joined}");
    assert_eq!(
        joined["conflicted"].as_array().unwrap().len(),
        0,
        "{joined}"
    );
    assert_eq!(frontier_count(&sides.reader, &sides.config), 1);
    assert_eq!(
        fs::read_to_string(reader.join("remote.txt")).unwrap(),
        "remote"
    );
    assert_eq!(
        fs::read_to_string(reader.join("local.txt")).unwrap(),
        "local"
    );
    let status = ok_json(reader, stores, &sides.config, &["status"]);
    assert_eq!(status["changes"].as_array().unwrap().len(), 0, "{status}");
    ok(reader, stores, &sides.config, &["validate"]);

    // Nothing is left to join, and the joined view publishes.
    let again = ok_json(reader, stores, &sides.config, &["integrate"]);
    assert_eq!(again["integrated"], false, "{again}");
    ok(reader, stores, &sides.config, &["publish"]);
    let (writer, writer_stores) = &sides.writer;
    ok(writer, writer_stores, &sides.config, &["fetch"]);
    assert_eq!(frontier_count(&sides.writer, &sides.config), 1);
}

/// Both sides add a file to the same published change. Integration combines the two
/// revisions into one that holds both files.
#[test]
fn both_sides_revising_one_change_combine() {
    let (sides, _server) = sides();
    let config = &sides.config;
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    let created = ok_json(
        writer,
        writer_stores,
        config,
        &["change", "new", "--message", "shared"],
    );
    let change = created["change"].as_str().unwrap().to_owned();
    ok(writer, writer_stores, config, &["publish"]);
    ok(reader, reader_stores, config, &["fetch"]);
    ok(reader, reader_stores, config, &["refresh"]);
    ok(
        reader,
        reader_stores,
        config,
        &["change", "select", &change],
    );

    fs::write(writer.join("writer.txt"), "writer").unwrap();
    ok(writer, writer_stores, config, &["add", "writer.txt"]);
    ok(writer, writer_stores, config, &["publish"]);
    fs::write(reader.join("reader.txt"), "reader").unwrap();
    ok(reader, reader_stores, config, &["add", "reader.txt"]);
    ok(reader, reader_stores, config, &["fetch"]);
    assert_eq!(frontier_count(&sides.reader, config), 2);

    let joined = ok_json(reader, reader_stores, config, &["integrate"]);

    assert_eq!(joined["composed"], serde_json::json!([change]), "{joined}");
    assert_eq!(joined["combined"], serde_json::json!([change]), "{joined}");
    assert_eq!(
        joined["conflicted"].as_array().unwrap().len(),
        0,
        "{joined}"
    );
    assert_eq!(
        fs::read_to_string(reader.join("writer.txt")).unwrap(),
        "writer"
    );
    assert_eq!(
        fs::read_to_string(reader.join("reader.txt")).unwrap(),
        "reader"
    );
    ok(reader, reader_stores, config, &["validate"]);
    ok(reader, reader_stores, config, &["publish"]);
}

#[test]
fn work_on_one_path_joins_with_a_conflict_to_resolve() {
    let (sides, _server) = diverged([("same.txt", "remote"), ("same.txt", "local")]);
    let (reader, stores) = &sides.reader;

    let joined = ok_json(reader, stores, &sides.config, &["integrate"]);

    assert_eq!(joined["integrated"], true, "{joined}");
    assert_eq!(
        joined["conflicted"].as_array().unwrap().len(),
        1,
        "{joined}"
    );
    let conflicts = ok_json(reader, stores, &sides.config, &["conflicts"]);
    assert_eq!(
        conflicts["conflicts"].as_array().unwrap().len(),
        1,
        "{conflicts}"
    );
    ok(reader, stores, &sides.config, &["validate"]);
    assert_eq!(frontier_count(&sides.reader, &sides.config), 1);
}

#[test]
fn uncaptured_edits_block_integrate() {
    let (sides, _server) = diverged([("remote.txt", "remote"), ("local.txt", "local")]);
    let (reader, stores) = &sides.reader;
    ok(reader, stores, &sides.config, &["materialize"]);
    fs::write(reader.join("local.txt"), "edited").unwrap();

    let refused = fails(reader, stores, &sides.config, &["integrate"]);

    assert!(
        refused.contains("uncaptured tracked workspace edits"),
        "{refused}"
    );
    assert_eq!(frontier_count(&sides.reader, &sides.config), 2);
}

/// An untracked file where the joined view has one stops the workspace switch, not the
/// join: the error says how to switch once the file is out of the way.
#[test]
fn an_untracked_file_in_the_way_leaves_the_workspace_as_it_was() {
    let (sides, _server) = diverged([("remote.txt", "remote"), ("local.txt", "local")]);
    let (reader, stores) = &sides.reader;
    fs::write(reader.join("remote.txt"), "mine").unwrap();

    let refused = fails(reader, stores, &sides.config, &["integrate"]);

    assert!(
        refused.contains("wheat switch main --frontier"),
        "{refused}"
    );
    assert_eq!(
        fs::read_to_string(reader.join("remote.txt")).unwrap(),
        "mine"
    );
    assert_eq!(frontier_count(&sides.reader, &sides.config), 1);
    fs::remove_file(reader.join("remote.txt")).unwrap();
    let view = ok_json(reader, stores, &sides.config, &["frontier", "list"])["frontiers"][0]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        reader,
        stores,
        &sides.config,
        &["switch", "main", "--frontier", &view],
    );
    assert_eq!(
        fs::read_to_string(reader.join("remote.txt")).unwrap(),
        "remote"
    );
}

#[test]
fn integrate_takes_only_frontiers_of_the_label() {
    let (sides, _server) = diverged([("remote.txt", "remote"), ("local.txt", "local")]);
    let (reader, stores) = &sides.reader;
    let basis = ok_json(reader, stores, &sides.config, &["frontier", "list"])["basis"]
        .as_str()
        .unwrap()
        .to_owned();

    let refused = fails(
        reader,
        stores,
        &sides.config,
        &["integrate", "--frontier", &basis],
    );

    assert!(
        refused.contains("is not a descendant frontier"),
        "{refused}"
    );
}

/// With nothing local, a pull brings the remote's work into the workspace.
#[test]
fn pull_brings_the_remote_work_into_the_workspace() {
    let (sides, _server) = sides();
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    change(&sides.writer, &sides.config, "remote.txt", "remote");
    ok(writer, writer_stores, &sides.config, &["publish"]);

    let pulled = ok_json(reader, reader_stores, &sides.config, &["pull"]);

    assert_eq!(pulled["fetch"]["kept_local"], false, "{pulled}");
    assert_eq!(
        fs::read_to_string(reader.join("remote.txt")).unwrap(),
        "remote"
    );
    let status = ok_json(reader, reader_stores, &sides.config, &["status"]);
    assert_eq!(status["changes"].as_array().unwrap().len(), 0, "{status}");
}

/// With local work, a pull fetches, joins both sides and switches to the result.
#[test]
fn pull_joins_the_remote_work_with_the_local_work() {
    let (sides, _server) = sides();
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    change(&sides.writer, &sides.config, "remote.txt", "remote");
    ok(writer, writer_stores, &sides.config, &["publish"]);
    change(&sides.reader, &sides.config, "local.txt", "local");

    let pulled = ok_json(reader, reader_stores, &sides.config, &["pull"]);

    assert_eq!(pulled["fetch"]["kept_local"], true, "{pulled}");
    assert_eq!(pulled["integrate"]["integrated"], true, "{pulled}");
    assert_eq!(frontier_count(&sides.reader, &sides.config), 1);
    assert_eq!(
        fs::read_to_string(reader.join("remote.txt")).unwrap(),
        "remote"
    );
    assert_eq!(
        fs::read_to_string(reader.join("local.txt")).unwrap(),
        "local"
    );
    ok(reader, reader_stores, &sides.config, &["publish"]);
}

/// The writer's change was published first; the reader's stack, one change of it on
/// the same path, was joined with it on publish. The join has an edge from the
/// writer's view over two changes, the last conflicted and leaving the tree as it was,
/// so its view has the same tree as the one-change child and selects the same
/// application. Fetching that join into the writer's store, and integrating it, stays
/// valid.
#[test]
fn a_fetched_join_with_a_conflict_stays_valid() {
    let (sides, _server) = sides();
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    change(&sides.writer, &sides.config, "same.txt", "writer");
    ok(writer, writer_stores, &sides.config, &["publish"]);
    change(&sides.reader, &sides.config, "other.txt", "other");
    change(&sides.reader, &sides.config, "same.txt", "reader");
    ok(reader, reader_stores, &sides.config, &["publish"]);
    ok(writer, writer_stores, &sides.config, &["fetch"]);
    ok(writer, writer_stores, &sides.config, &["validate"]);

    let joined = ok_json(writer, writer_stores, &sides.config, &["integrate"]);

    assert_eq!(joined["integrated"], true, "{joined}");
    ok(writer, writer_stores, &sides.config, &["validate"]);
    assert_eq!(frontier_count(&sides.writer, &sides.config), 1);
}

/// A label made at a view with a conflict holds the conflicted change. Resolving it
/// revises that change while the other side adds work on the label: a pull joins both,
/// the resolution included.
#[test]
fn pull_takes_a_resolution_of_a_conflict_the_label_holds() {
    let (sides, _server) = diverged([("same.txt", "remote"), ("same.txt", "local")]);
    let config = &sides.config;
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    let joined = ok_json(reader, reader_stores, config, &["integrate"]);
    assert_eq!(
        joined["conflicted"].as_array().unwrap().len(),
        1,
        "{joined}"
    );
    ok(reader, reader_stores, config, &["label", "create", "dev"]);
    ok(reader, reader_stores, config, &["switch", "dev"]);
    ok(reader, reader_stores, config, &["publish"]);
    ok(writer, writer_stores, config, &["fetch"]);
    ok(writer, writer_stores, config, &["switch", "dev"]);

    ok(
        reader,
        reader_stores,
        config,
        &["resolve", "--path", "same.txt", "--value", "settled"],
    );
    change(&sides.writer, config, "more.txt", "more");
    ok(writer, writer_stores, config, &["publish"]);
    assert_eq!(frontier_count(&sides.reader, config), 1);

    let pulled = ok_json(reader, reader_stores, config, &["pull"]);

    assert_eq!(pulled["integrate"]["integrated"], true, "{pulled}");
    assert_eq!(
        pulled["integrate"]["conflicted"].as_array().unwrap().len(),
        0,
        "{pulled}"
    );
    for (path, value) in [("same.txt", "settled"), ("more.txt", "more")] {
        assert_eq!(
            fs::read_to_string(reader.join(path)).unwrap(),
            value,
            "{path}"
        );
    }
    let conflicts = ok_json(reader, reader_stores, config, &["conflicts"]);
    assert_eq!(
        conflicts["conflicts"].as_array().unwrap().len(),
        0,
        "{conflicts}"
    );
    assert_eq!(frontier_count(&sides.reader, config), 1);
    ok(reader, reader_stores, config, &["validate"]);
    ok(reader, reader_stores, config, &["publish"]);
}

/// A pull whose integrate is refused keeps the fetch and says what to run next.
#[test]
fn a_refused_integrate_keeps_the_pulled_fetch() {
    let (sides, _server) = sides();
    let (writer, writer_stores) = &sides.writer;
    let (reader, reader_stores) = &sides.reader;
    change(&sides.writer, &sides.config, "remote.txt", "remote");
    ok(writer, writer_stores, &sides.config, &["publish"]);
    change(&sides.reader, &sides.config, "local.txt", "local");
    ok(reader, reader_stores, &sides.config, &["materialize"]);
    fs::write(reader.join("local.txt"), "edited").unwrap();

    let refused = fails(reader, reader_stores, &sides.config, &["pull"]);

    assert!(refused.contains("the fetch is kept"), "{refused}");
    assert!(
        refused.contains("uncaptured tracked workspace edits"),
        "{refused}"
    );
    assert_eq!(frontier_count(&sides.reader, &sides.config), 2);
}
