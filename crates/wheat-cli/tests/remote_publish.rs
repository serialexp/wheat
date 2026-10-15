#![cfg(target_os = "linux")]

//! `wheat publish` when the remote moved since the last fetch: the server joins the
//! publish with what was published in between, as a fetch would, and refuses only
//! where the two sides really disagree.

mod remote_support;

use std::path::{Path, PathBuf};

use remote_support::{Server, fails, ok, ok_json, serve, write_config};

/// Clones of one server, each with a store home of its own.
struct Clients {
    root: PathBuf,
    config: PathBuf,
    server: Server,
}

impl Clients {
    fn new(root: &Path) -> Self {
        let config = root.join("config");
        write_config(&config);
        let server = serve(root, &config);
        Self {
            root: root.to_owned(),
            config,
            server,
        }
    }

    /// Clones the server into workspace `name`; returns it and its store home.
    fn clone(&self, name: &str) -> (PathBuf, PathBuf) {
        let stores = self.root.join(format!("{name}-stores"));
        let workspace = self.root.join(name);
        ok(
            &self.root,
            &stores,
            &self.config,
            &["clone", &self.server.url, workspace.to_str().unwrap()],
        );
        (workspace, stores)
    }

    /// Adds a change in `workspace` that writes `<name>.txt`.
    fn add_change(&self, (workspace, stores): &(PathBuf, PathBuf), name: &str) {
        ok(
            workspace,
            stores,
            &self.config,
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

    fn json(&self, (workspace, stores): &(PathBuf, PathBuf), args: &[&str]) -> serde_json::Value {
        ok_json(workspace, stores, &self.config, args)
    }

    fn frontiers(&self, client: &(PathBuf, PathBuf)) -> Vec<String> {
        let listed = self.json(client, &["frontier", "list"]);
        let mut frontiers: Vec<String> = listed["frontiers"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .map(|view| view.as_str().unwrap().to_owned())
            .collect();
        frontiers.sort();
        frontiers
    }
}

/// Two writers start from the same head and publish unrelated changes. The second
/// needs no fetch first: the server joins its change with the first, and the
/// writer ends up holding the joined head, as if it had fetched afterwards.
#[test]
fn a_publish_behind_the_remote_is_joined_with_it() {
    let root = tempfile::tempdir().unwrap();
    let clients = Clients::new(root.path());
    let first = clients.clone("first");
    let second = clients.clone("second");

    clients.add_change(&first, "one");
    let published = clients.json(&first, &["publish"]);
    assert_eq!(published["joined"], false, "{published}");

    clients.add_change(&second, "two");
    let published = clients.json(&second, &["publish"]);
    assert_eq!(published["joined"], true, "{published}");
    assert_eq!(published["generation"], 2, "{published}");

    // The second writer holds the joined head and is up to date with the server.
    ok(&second.0, &second.1, &clients.config, &["validate"]);
    let again = clients.json(&second, &["fetch"]);
    assert_eq!(again["up_to_date"], true, "{again}");
    let joined = clients.frontiers(&second);
    assert_eq!(joined.len(), 2, "{joined:?}");

    // The first writer fetches the same history.
    let fetched = clients.json(&first, &["fetch"]);
    assert_eq!(fetched["kept_local"], false, "{fetched}");
    ok(&first.0, &first.1, &clients.config, &["validate"]);
    assert_eq!(clients.frontiers(&first), joined);
}

/// A label both writers moved cannot be joined: the publish is sent back as stale
/// and the server's head stays where the first writer left it.
#[test]
fn a_label_moved_on_both_sides_refuses_the_publish() {
    let root = tempfile::tempdir().unwrap();
    let clients = Clients::new(root.path());
    let first = clients.clone("first");
    let second = clients.clone("second");
    let label_tip = |client: &(PathBuf, PathBuf), name: &str| {
        clients.add_change(client, name);
        let tip = clients.frontiers(client).pop().unwrap();
        ok(
            &client.0,
            &client.1,
            &clients.config,
            &["label", "create", "shared", "--view", &tip],
        );
    };
    label_tip(&first, "one");
    ok(&first.0, &first.1, &clients.config, &["publish"]);
    label_tip(&second, "two");

    let refused = fails(&second.0, &second.1, &clients.config, &["publish"]);
    assert!(
        refused.contains("labels were moved differently"),
        "{refused}"
    );
    assert!(refused.contains("shared"), "{refused}");

    // The first writer last saw the head it published, and it is still the head.
    let unchanged = clients.json(&first, &["fetch"]);
    assert_eq!(unchanged["up_to_date"], true, "{unchanged}");
}
