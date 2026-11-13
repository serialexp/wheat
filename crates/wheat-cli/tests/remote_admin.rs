#![cfg(target_os = "linux")]

//! `wheat admin` against a server run with a repositories directory: the maintenance
//! operations a person gives through the real interface.

use crate::remote_support;

use std::fs;
use std::path::{Path, PathBuf};

use remote_support::{Server, fails, ok, ok_json, serve_repos_dir_configured, write_config};
use wheat_server::access::{Access, AuthConfig, PrincipalConfig, token_hash_hex};
use wheat_server::config::FileConfig;
use wheat_server::server::ServerConfig;
use wheat_store::RepositoriesDirectory;

/// A server over a new repositories directory `name` under `root`, exchanging bundles
/// through `exchange`, with `team/repo` in it.
fn serve(root: &Path, name: &str, exchange: &Path, server: ServerConfig) -> (Server, PathBuf) {
    let repos = root.join(name);
    fs::create_dir(&repos).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(RepositoriesDirectory::new(&repos).create_empty("team/repo", "main"))
        .unwrap();
    let mut server = server;
    server.exchange_dir = Some(exchange.to_path_buf());
    (serve_repos_dir_configured(&repos, server), repos)
}

fn clone(root: &Path, config: &Path, url: &str, name: &str) -> (PathBuf, PathBuf) {
    let stores = root.join(format!("{name}-stores"));
    let workspace = root.join(name);
    ok(
        root,
        &stores,
        config,
        &["clone", url, workspace.to_str().unwrap()],
    );
    (workspace, stores)
}

fn publish(config: &Path, (workspace, stores): &(PathBuf, PathBuf), name: &str) {
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
    ok(workspace, stores, config, &["publish"]);
}

/// The `ID=NAME` arguments for importing what an export's message lists.
fn names_of(message: &str) -> Vec<String> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix("repository "))
        .map(|rest| rest.replacen(' ', "=", 1))
        .collect()
}

/// A network exported from one server and imported into another is served there
/// with its history, every repository under the name it was given.
#[test]
fn a_network_exported_by_one_server_is_imported_by_another() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let (first, first_repos) = serve(root.path(), "first", &exchange, ServerConfig::new());
    let first_url = |name: &str| format!("wheat://{}/{name}", first.addr);
    let parent = clone(root.path(), &config, &first_url("team/repo"), "parent");
    publish(&config, &parent, "one");
    RepositoriesDirectory::new(&first_repos)
        .fork("team/repo", "fork/repo")
        .unwrap();
    let fork = clone(root.path(), &config, &first_url("fork/repo"), "fork");
    publish(&config, &fork, "two");

    let server = format!("wheat://{}", first.addr);
    let exported = ok_json(
        root.path(),
        &stores,
        &config,
        &[
            "admin",
            "--server",
            &server,
            "export",
            "team/repo",
            "bundle",
        ],
    );
    let message = exported["message"].as_str().unwrap();
    assert!(exported["message"].is_string());
    assert!(
        exchange.join("bundle/bundle.manifest").is_file(),
        "{message}"
    );
    let names = names_of(message);
    assert_eq!(names.len(), 2, "{message}");

    // Into a server that holds nothing of it; its own `team/repo` is another network.
    let second_repos = root.path().join("second");
    fs::create_dir(&second_repos).unwrap();
    let mut second_config = ServerConfig::new();
    second_config.exchange_dir = Some(exchange.clone());
    let second = serve_repos_dir_configured(&second_repos, second_config);
    let second_server = format!("wheat://{}", second.addr);
    let mut args = vec!["admin", "--server", &second_server, "import", "bundle"];
    for name in &names {
        args.extend(["--name", name]);
    }
    let imported = ok_json(root.path(), &stores, &config, &args);
    assert!(
        imported["message"].as_str().unwrap().contains("imported"),
        "{imported}"
    );

    let second_url = |name: &str| format!("wheat://{}/{name}", second.addr);
    let again = clone(root.path(), &config, &second_url("team/repo"), "again");
    assert_eq!(fs::read_to_string(again.0.join("one.txt")).unwrap(), "one");
    let again_fork = clone(root.path(), &config, &second_url("fork/repo"), "again-fork");
    assert_eq!(
        fs::read_to_string(again_fork.0.join("two.txt")).unwrap(),
        "two"
    );
}

/// A standalone repository directory in the exchange directory becomes a repository.
#[test]
fn a_standalone_repository_is_adopted() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let source = root.path().join("source");
    ok(
        root.path(),
        &root.path().join("source-stores"),
        &config,
        &["init", source.to_str().unwrap()],
    );
    let store = remote_support::workspace_store(&source);
    fs::rename(&store, exchange.join("standalone")).unwrap();

    let (server, _) = serve(root.path(), "repos", &exchange, ServerConfig::new());
    let url = format!("wheat://{}", server.addr);
    ok(
        root.path(),
        &stores,
        &config,
        &[
            "admin",
            "--server",
            &url,
            "adopt",
            "standalone",
            "adopted/repo",
        ],
    );
    let adopted = clone(
        root.path(),
        &config,
        &format!("wheat://{}/adopted/repo", server.addr),
        "adopted",
    );
    assert!(adopted.0.join(".wheat").is_dir());
}

/// The server only touches names inside its exchange directory, and only when it has
/// one.
#[test]
fn bundles_outside_the_exchange_directory_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let (server, _) = serve(root.path(), "repos", &exchange, ServerConfig::new());
    let url = format!("wheat://{}", server.addr);
    for escaping in ["../out", "/tmp/out", ".hidden", "a/b"] {
        let stderr = fails(
            root.path(),
            &stores,
            &config,
            &["admin", "--server", &url, "export", "team/repo", escaping],
        );
        assert!(stderr.contains("exchange"), "{escaping}: {stderr}");
    }
    assert!(!root.path().join("out").exists());

    // Without --exchange-dir nothing is exchanged.
    let repos = root.path().join("plain");
    fs::create_dir(&repos).unwrap();
    let plain = serve_repos_dir_configured(&repos, ServerConfig::new());
    let stderr = fails(
        root.path(),
        &stores,
        &config,
        &[
            "admin",
            "--server",
            &format!("wheat://{}", plain.addr),
            "export",
            "team/repo",
            "bundle",
        ],
    );
    assert!(stderr.contains("exchange"), "{stderr}");
}

/// A principal that is not an admin may read repositories but not run operations.
#[test]
fn only_an_admin_principal_may_run_operations() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let principal = |name: &str, admin: bool| PrincipalConfig {
        name: name.to_owned(),
        token_hashes: vec![token_hash_hex(format!("{name}-token").as_bytes())],
        repositories: [("team/repo".to_owned(), Access::Publish)].into(),
        admin,
    };
    let server_config = ServerConfig::from_file(&FileConfig {
        auth: Some(AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal("operator", true), principal("reader", false)],
        }),
        ..FileConfig::default()
    })
    .unwrap();
    let (server, _) = serve(root.path(), "repos", &exchange, server_config);
    let url = format!("wheat://{}", server.addr);

    let as_user = |name: &str| {
        ok(
            root.path(),
            &stores,
            &config,
            &[
                "config",
                "set",
                "--global",
                "credential.helper",
                &format!("printf 'token={name}-token\\n' #"),
            ],
        );
    };
    let export = ["admin", "--server", &url, "export", "team/repo", "bundle"];

    as_user("reader");
    let stderr = fails(root.path(), &stores, &config, &export);
    assert!(stderr.contains("admin"), "{stderr}");
    assert!(!exchange.join("bundle").exists());

    as_user("operator");
    ok(root.path(), &stores, &config, &export);
    assert!(exchange.join("bundle/bundle.manifest").is_file());
}

/// A move ends with deleting the source: the server stops serving the repositories and
/// needs no exchange directory for it, while the copy elsewhere carries on.
#[test]
fn deleting_a_served_repository_ends_a_move() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let (first, first_repos) = serve(root.path(), "first", &exchange, ServerConfig::new());
    let first_url = |name: &str| format!("wheat://{}/{name}", first.addr);
    let parent = clone(root.path(), &config, &first_url("team/repo"), "parent");
    publish(&config, &parent, "one");
    RepositoriesDirectory::new(&first_repos)
        .fork("team/repo", "fork/repo")
        .unwrap();

    let server = format!("wheat://{}", first.addr);
    let exported = ok_json(
        root.path(),
        &stores,
        &config,
        &[
            "admin",
            "--server",
            &server,
            "export",
            "team/repo",
            "bundle",
        ],
    );
    let names = names_of(exported["message"].as_str().unwrap());

    let second_repos = root.path().join("second");
    fs::create_dir(&second_repos).unwrap();
    let mut second_config = ServerConfig::new();
    second_config.exchange_dir = Some(exchange.clone());
    let second = serve_repos_dir_configured(&second_repos, second_config);
    let second_server = format!("wheat://{}", second.addr);
    let mut args = vec!["admin", "--server", &second_server, "import", "bundle"];
    for name in &names {
        args.extend(["--name", name]);
    }
    ok(root.path(), &stores, &config, &args);

    // The source's repositories are open (the clone above, the export) when they go.
    for name in ["fork/repo", "team/repo"] {
        ok(
            root.path(),
            &stores,
            &config,
            &["admin", "--server", &server, "delete", name],
        );
    }
    assert!(
        fails(
            root.path(),
            &stores,
            &config,
            &[
                "clone",
                &first_url("team/repo"),
                root.path().join("gone").to_str().unwrap()
            ],
        )
        .contains("not found")
    );
    assert!(
        !first_repos
            .join("networks")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );
    let moved = clone(
        root.path(),
        &config,
        &format!("wheat://{}/team/repo", second.addr),
        "moved",
    );
    assert_eq!(fs::read_to_string(moved.0.join("one.txt")).unwrap(), "one");

    // Not there any more.
    let stderr = fails(
        root.path(),
        &stores,
        &config,
        &["admin", "--server", &server, "delete", "team/repo"],
    );
    assert!(stderr.contains("no such repository"), "{stderr}");
}
/// A dry run counts the objects of superseded publishes that no head reaches, and
/// changes nothing; the collection removes them, and the repository serves the same
/// afterwards.
#[test]
fn gc_removes_what_publishes_left_unreachable() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let exchange = root.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    let stores = root.path().join("admin-stores");

    let (server, repos) = serve(root.path(), "repos", &exchange, ServerConfig::new());
    let url = format!("wheat://{}", server.addr);
    let workspace = clone(root.path(), &config, &format!("{url}/team/repo"), "work");
    for name in ["one", "two", "three"] {
        publish(&config, &workspace, name);
    }

    let gc = ["admin", "--server", &url, "gc", "team/repo"];
    let counted = ok_json(
        root.path(),
        &stores,
        &config,
        &[gc.as_slice(), &["--dry-run"]].concat(),
    );
    let message = counted["message"].as_str().unwrap();
    assert!(message.contains("nothing was changed"), "{message}");
    assert!(message.contains("RepositoryEnvelope"), "{message}");
    let unreachable = message
        .lines()
        .find(|line| line.contains("are reached by none"))
        .unwrap();
    assert!(
        !unreachable.contains("; 0 objects"),
        "three publishes leave garbage: {message}"
    );
    // Counting again finds the same: it deleted nothing.
    let again = ok_json(
        root.path(),
        &stores,
        &config,
        &[gc.as_slice(), &["--dry-run"]].concat(),
    );
    assert_eq!(again["message"], counted["message"]);
    assert!(repos.join("networks").is_dir());

    let collected = ok_json(root.path(), &stores, &config, &gc);
    let message = collected["message"].as_str().unwrap();
    assert!(message.contains("1 heads validated"), "{message}");
    assert!(!message.contains(", 0 objects ("), "{message}");
    let after = ok_json(
        root.path(),
        &stores,
        &config,
        &[gc.as_slice(), &["--dry-run"]].concat(),
    );
    let message = after["message"].as_str().unwrap();
    assert!(
        message
            .lines()
            .any(|line| line.contains("are reached by none") && line.contains("; 0 objects")),
        "the collection left garbage: {message}"
    );
    // The repository serves what it did, and takes publishes again.
    let fresh = clone(root.path(), &config, &format!("{url}/team/repo"), "fresh");
    for name in ["one", "two", "three"] {
        assert_eq!(
            fs::read_to_string(fresh.0.join(format!("{name}.txt"))).unwrap(),
            name
        );
    }
    publish(&config, &workspace, "four");
}
