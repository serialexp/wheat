#![cfg(target_os = "linux")]

//! Holding a served repository still to copy it: publishes are refused for the
//! moment with a retryable error, fetches go on, the copy serves what was published,
//! and publishes are taken again once the hold ends.

use crate::remote_support;

use remote_support::sides::sides;
use remote_support::{ok, serve_store};
use wheat_store::RepositoryDirectory;

#[test]
fn a_quiesced_repository_refuses_publishes_serves_fetches_and_copies_whole() {
    let (sides, server) = sides();
    let config = &sides.config;
    sides.writer.change(config, "first.txt");
    sides.writer.ok_json(config, &["publish"]);
    let published = sides.writer.label_view(config, "main").unwrap();

    let quiesced = server.quiesce();

    sides.writer.change(config, "second.txt");
    let refused = sides.writer.fails_json(config, &["publish"]);
    assert_eq!(refused["error"]["code"], "remote_unavailable", "{refused}");
    assert_eq!(refused["error"]["retryable"], true, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("publish again shortly"),
        "{refused}"
    );
    assert!(
        !refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expects"),
        "a hold nobody gave an estimate for names none: {refused}"
    );
    sides.reader.ok_json(config, &["fetch"]);
    assert_eq!(sides.reader.label_view(config, "main").unwrap(), published);

    // A holder that expects the hold to last says so to those it refuses.
    quiesced.expect(std::time::Duration::from_secs(120));
    let refused = sides.writer.fails_json(config, &["publish"]);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("the server expects this to last about 1"),
        "{refused}"
    );

    // Copied while held, the repository is whole: serve the copy and clone it.
    let copy = RepositoryDirectory::new(sides.root.path().join("copy"));
    RepositoryDirectory::new(&server.store)
        .copy_to(&copy)
        .unwrap();
    let copy_server = serve_store(copy.root());
    let clone = sides.root.path().join("from-copy");
    let clone_stores = sides.root.path().join("from-copy-stores");
    ok(
        sides.root.path(),
        &clone_stores,
        config,
        &["clone", &copy_server.url, clone.to_str().unwrap()],
    );
    let copied = remote_support::sides::Side {
        workspace: clone,
        stores: clone_stores,
    };
    assert_eq!(copied.label_view(config, "main").unwrap(), published);

    drop(quiesced);
    sides.writer.ok_json(config, &["publish"]);
    assert_ne!(sides.writer.label_view(config, "main").unwrap(), published);
}
