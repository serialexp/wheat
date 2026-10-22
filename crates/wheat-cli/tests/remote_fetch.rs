#![cfg(target_os = "linux")]

//! `wheat clone` gets a whole history from the server's push, and `wheat fetch`
//! afterwards gets only what changed since the last fetch.

mod remote_support;

use std::fs;
use std::path::Path;

use remote_support::{fails, git, ok, ok_json, serve, serve_store, workspace_store, write_config};

const COMMITS: usize = 80;

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

/// Each durable append costs several fsyncs, so a fetch stores what it receives and
/// what its merge with local work makes together, not once per message or update:
/// one append for the fetched content and the objects.
#[test]
fn a_fetch_that_keeps_local_work_stores_it_in_one_append() {
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
    for name in ["a/b/one", "c/two", "three"] {
        add_change(&writer, &writer_stores, &config, name);
    }
    ok(&writer, &writer_stores, &config, &["publish"]);
    add_change(&reader, &reader_stores, &config, "d/local");

    let trace = root.join("fetch-trace.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(&reader)
        .args(["--format", "json", "fetch"])
        .env("WHEAT_STORE_HOME", &reader_stores)
        .env("XDG_CONFIG_HOME", &config)
        .env("WHEAT_TRACE", &trace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fetched: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(fetched["result"]["kept_local"], true, "{fetched}");

    let document: serde_json::Value = serde_json::from_slice(&fs::read(&trace).unwrap()).unwrap();
    let appends = document["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["name"] == "packed_append")
        .count();
    assert_eq!(appends, 1);
    ok(&reader, &reader_stores, &config, &["validate"]);
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

/// Points the head of `store` at a forged state: its own, except that the retained
/// changes leave out the changes its change map names.
fn forge_unretained_changes(store: &Path) {
    use wheat_core::RepositoryEnvelopeRecord;
    use wheat_store::{
        ImmutableObjectStore, LocalPublicationStore, PackedObjectStore, PublicationStore,
        RepositoryReader, RetainedSetStore,
    };

    let heads = LocalPublicationStore::new(store.join("repository.head"));
    let objects = PackedObjectStore::open(store.join("storage")).unwrap();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    let head = heads.read().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let forged = runtime.block_on(async {
        let envelope = reader.envelope(head.envelope).await.unwrap();
        let mut state = reader.state(envelope.state).await.unwrap();
        state.retained_changes = RetainedSetStore::new(&objects).empty_root().await.unwrap();
        objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await
            .unwrap();
        let forged = RepositoryEnvelopeRecord {
            state: state.id(),
            labels: envelope.labels,
        };
        objects
            .put_if_absent(forged.id().into(), &forged.canonical_bytes())
            .await
            .unwrap();
        forged.id()
    });
    heads
        .compare_and_advance(head, head.next(forged).unwrap())
        .unwrap();
}

/// Nothing a clone receives is trusted, so it checks the whole head before keeping it.
#[test]
fn a_clone_refuses_a_forged_head() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let config = root.join("config");
    write_config(&config);
    let server_stores = root.join("server-stores");
    let source = root.join("source");
    ok(
        root,
        &server_stores,
        &config,
        &["init", source.to_str().unwrap()],
    );
    add_change(&source, &server_stores, &config, "forged");
    let store = workspace_store(&source);
    forge_unretained_changes(&store);
    let server = serve_store(&store);

    let reader = root.join("reader");
    let refused = fails(
        root,
        &root.join("reader-stores"),
        &config,
        &["clone", &server.url, reader.to_str().unwrap()],
    );
    assert!(
        refused.contains("the remote's head failed validation"),
        "{refused}"
    );
}

/// A forced publish can drop history a reader already fetched. A fetch checks only
/// what the remote added to its last fetch, so it refuses the rewritten head and says
/// how to take it; `--force` checks the whole head and takes it.
#[test]
fn a_fetch_refuses_a_rewritten_remote_unless_forced() {
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
    add_change(&writer, &writer_stores, &config, "dropped");
    ok(&writer, &writer_stores, &config, &["publish"]);
    ok(&reader, &reader_stores, &config, &["fetch"]);

    // An unrelated history, which lacks the reader's fetched change, replaces it.
    let other_stores = root.join("other-stores");
    let other = root.join("other");
    ok(
        root,
        &other_stores,
        &config,
        &["init", other.to_str().unwrap()],
    );
    add_change(&other, &other_stores, &config, "kept");
    ok(
        &other,
        &other_stores,
        &config,
        &["publish", "--remote", &server.url, "--force"],
    );

    let refused = fails(&reader, &reader_stores, &config, &["fetch"]);
    assert!(
        refused.contains("the remote's head failed validation"),
        "{refused}"
    );
    assert!(refused.contains("wheat fetch --force"), "{refused}");
    let forced = ok_json(&reader, &reader_stores, &config, &["fetch", "--force"]);
    assert_eq!(forced["up_to_date"], false, "{forced}");
    ok(&reader, &reader_stores, &config, &["validate"]);
}
