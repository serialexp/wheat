#![cfg(target_os = "linux")]

//! `wheat clone` gets a whole history from the server's push, and `wheat fetch`
//! afterwards gets only what changed since the last fetch.

mod remote_support;

use std::fs;
use std::path::Path;

use remote_support::{fails, git, ok, ok_json, serve, write_config};

const COMMITS: usize = 60;

/// Commits `COMMITS` changes to a Git repository under `root`, one file each, and
/// returns it.
fn git_history(root: &Path) -> std::path::PathBuf {
    let git_work = root.join("git-work");
    fs::create_dir_all(&git_work).unwrap();
    git(&git_work, &["init", "-q", "-b", "main"]);
    for commit in 0..COMMITS {
        let path = git_work.join(format!("d{}/f{commit}.txt", commit % 5));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("commit {commit}\n")).unwrap();
        git(&git_work, &["add", "-A"]);
        git(
            &git_work,
            &["commit", "-q", "-m", &format!("commit {commit}")],
        );
    }
    git_work
}

#[test]
fn a_clone_needs_no_wants_and_a_fetch_brings_only_the_change() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);

    // A writer imports a Git history and publishes it.
    let git_work = git_history(root);
    let writer_stores = root.join("writer-stores");
    let writer = root.join("writer");
    ok(
        root,
        &writer_stores,
        &config,
        &[
            "import",
            git_work.to_str().unwrap(),
            writer.to_str().unwrap(),
        ],
    );
    // The imported history is unrelated to the server's, hence --force.
    ok(
        &writer,
        &writer_stores,
        &config,
        &["publish", "--remote", &server.url, "--force"],
    );

    // A reader clones it: everything comes in the push.
    let reader_stores = root.join("reader-stores");
    let reader = root.join("reader");
    let cloned = ok_json(
        root,
        &reader_stores,
        &config,
        &["clone", &server.url, reader.to_str().unwrap()],
    );
    assert_eq!(cloned["pulled"], 0, "{cloned}");
    assert_eq!(cloned["wants"], 0, "{cloned}");
    let clone_objects = cloned["objects"].as_u64().unwrap();
    assert!(clone_objects > 3 * 256, "{cloned}");
    ok(&reader, &reader_stores, &config, &["validate"]);

    // The writer adds one change; the reader fetches just that.
    ok(
        &writer,
        &writer_stores,
        &config,
        &[
            "change",
            "new",
            "--path",
            "late.txt",
            "--value",
            "late",
            "--message",
            "one more",
        ],
    );
    ok(&writer, &writer_stores, &config, &["publish"]);
    let fetched = ok_json(&reader, &reader_stores, &config, &["fetch"]);
    assert_eq!(fetched["up_to_date"], false, "{fetched}");
    assert_eq!(fetched["incremental"], true, "{fetched}");
    assert_eq!(fetched["pulled"], 0, "{fetched}");
    assert_eq!(fetched["content"], 1, "{fetched}");
    // Only the content the last fetch lacked is looked up, not all 61 files.
    assert_eq!(fetched["content_checked"], 1, "{fetched}");
    let fetch_objects = fetched["objects"].as_u64().unwrap();
    assert!(
        fetch_objects * 10 < clone_objects,
        "one change should bring a small part of the history: {fetched}"
    );
    ok(&reader, &reader_stores, &config, &["validate"]);

    // Nothing new: nothing moves.
    let again = ok_json(&reader, &reader_stores, &config, &["fetch"]);
    assert_eq!(again["up_to_date"], true, "{again}");
}

/// Adds a change in `workspace` that writes `name` to `<name>.txt`.
fn add_change(workspace: &Path, stores: &Path, config: &Path, name: &str) {
    ok(
        workspace,
        stores,
        config,
        &[
            "change",
            "new",
            "--path",
            &format!("{name}.txt"),
            "--value",
            name,
            "--message",
            name,
        ],
    );
}

/// The local head and the remote count their own generations, so a fetch may
/// cover several remote generations, and publishing several local changes at
/// once leaves nothing unpublished.
#[test]
fn a_fetch_catches_up_on_several_publishes_and_knows_its_own() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);
    let clone = |name: &str| {
        let stores = root.join(format!("{name}-stores"));
        let workspace = root.join(name);
        ok(
            root,
            &stores,
            &config,
            &["clone", &server.url, workspace.to_str().unwrap()],
        );
        (workspace, stores)
    };
    let (writer, writer_stores) = clone("writer");
    let (reader, reader_stores) = clone("reader");

    // Two local changes in one publish: afterwards the writer is up to date.
    add_change(&writer, &writer_stores, &config, "a");
    add_change(&writer, &writer_stores, &config, "b");
    ok(&writer, &writer_stores, &config, &["publish"]);
    let own = ok_json(&writer, &writer_stores, &config, &["fetch"]);
    assert_eq!(own["up_to_date"], true, "{own}");

    // The reader catches up on both publishes in one fetch.
    add_change(&writer, &writer_stores, &config, "c");
    ok(&writer, &writer_stores, &config, &["publish"]);
    let fetched = ok_json(&reader, &reader_stores, &config, &["fetch"]);
    assert_eq!(fetched["up_to_date"], false, "{fetched}");
    assert_eq!(fetched["incremental"], true, "{fetched}");
    assert_eq!(fetched["content"], 3, "{fetched}");
    assert_eq!(fetched["content_checked"], 3, "{fetched}");
    ok(&reader, &reader_stores, &config, &["validate"]);
}

fn frontiers(workspace: &Path, stores: &Path, config: &Path) -> Vec<String> {
    let listed = ok_json(workspace, stores, config, &["frontier", "list"]);
    let mut frontiers: Vec<String> = listed["frontiers"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"))
        .iter()
        .map(|view| {
            view.as_str()
                .unwrap_or_else(|| panic!("{listed}"))
                .to_owned()
        })
        .collect();
    frontiers.sort();
    frontiers
}

/// Work on both sides: a fetch keeps the local change beside the remote's, the result
/// publishes as a successor of the remote, and the other side fetches it back.
#[test]
fn a_fetch_keeps_local_work_beside_the_remotes() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);
    let clone = |name: &str| {
        let stores = root.join(format!("{name}-stores"));
        let workspace = root.join(name);
        ok(
            root,
            &stores,
            &config,
            &["clone", &server.url, workspace.to_str().unwrap()],
        );
        (workspace, stores)
    };
    let (writer, writer_stores) = clone("writer");
    let (reader, reader_stores) = clone("reader");

    add_change(&writer, &writer_stores, &config, "remote");
    ok(&writer, &writer_stores, &config, &["publish"]);
    add_change(&reader, &reader_stores, &config, "local");
    let local_only = frontiers(&reader, &reader_stores, &config);
    assert_eq!(local_only.len(), 1, "{local_only:?}");

    let fetched = ok_json(&reader, &reader_stores, &config, &["fetch"]);
    assert_eq!(fetched["up_to_date"], false, "{fetched}");
    assert_eq!(fetched["kept_local"], true, "{fetched}");
    ok(&reader, &reader_stores, &config, &["validate"]);
    let joined = frontiers(&reader, &reader_stores, &config);
    assert_eq!(joined.len(), 2, "{joined:?}");
    assert!(joined.contains(&local_only[0]), "{joined:?}");

    // Fetching again changes nothing, and the joined history publishes.
    let again = ok_json(&reader, &reader_stores, &config, &["fetch"]);
    assert_eq!(again["up_to_date"], true, "{again}");
    ok(&reader, &reader_stores, &config, &["publish"]);

    let back = ok_json(&writer, &writer_stores, &config, &["fetch"]);
    assert_eq!(back["kept_local"], false, "{back}");
    ok(&writer, &writer_stores, &config, &["validate"]);
    assert_eq!(frontiers(&writer, &writer_stores, &config), joined);
}

/// `--force` drops the local work and takes the remote as it is.
#[test]
fn a_forced_fetch_drops_local_work() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);
    let stores = root.join("stores");
    let workspace = root.join("workspace");
    ok(
        root,
        &stores,
        &config,
        &["clone", &server.url, workspace.to_str().unwrap()],
    );
    let published = frontiers(&workspace, &stores, &config);

    add_change(&workspace, &stores, &config, "dropped");
    assert_ne!(frontiers(&workspace, &stores, &config), published);
    let forced = ok_json(&workspace, &stores, &config, &["fetch", "--force"]);
    assert_eq!(forced["kept_local"], false, "{forced}");
    assert_eq!(frontiers(&workspace, &stores, &config), published);
}

/// A label moved differently on each side stops the fetch and names it.
#[test]
fn a_label_moved_on_both_sides_stops_the_fetch() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);
    let clone = |name: &str| {
        let stores = root.join(format!("{name}-stores"));
        let workspace = root.join(name);
        ok(
            root,
            &stores,
            &config,
            &["clone", &server.url, workspace.to_str().unwrap()],
        );
        (workspace, stores)
    };
    let (writer, writer_stores) = clone("writer");
    let (reader, reader_stores) = clone("reader");
    let label_tip = |workspace: &Path, stores: &Path, name: &str| {
        add_change(workspace, stores, &config, name);
        let tip = frontiers(workspace, stores, &config).pop().unwrap();
        ok(
            workspace,
            stores,
            &config,
            &["label", "create", "shared", "--view", &tip],
        );
    };
    label_tip(&writer, &writer_stores, "remote");
    ok(&writer, &writer_stores, &config, &["publish"]);
    label_tip(&reader, &reader_stores, "local");

    let refused = fails(&reader, &reader_stores, &config, &["fetch"]);
    assert!(
        refused.contains("labels were moved differently"),
        "{refused}"
    );
    assert!(refused.contains("shared"), "{refused}");
}
