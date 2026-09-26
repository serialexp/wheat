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
    ok(&reader, &reader_stores, &config, &["validate"]);

    // A local change the remote doesn't have still stops a fetch.
    add_change(&reader, &reader_stores, &config, "d");
    let refused = fails(&reader, &reader_stores, &config, &["fetch"]);
    assert!(refused.contains("unpublished changes"), "{refused}");
}
