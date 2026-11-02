#![cfg(target_os = "linux")]

//! An embedder's publish hooks, around real publishes: the check sees what the sender
//! asks for before it joins the head and can refuse it, and the durable batch is
//! announced once the head has moved.

use crate::remote_support;

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use remote_support::sides::sides_configured;
use wheat_server::hooks::{ProposedPublish, PublishHooks, PublishRefusal, PublishedBatch};
use wheat_server::server::ServerConfig;

/// How long a test waits for a batch's notice before calling it lost.
const NOTICE_WAIT: Duration = Duration::from_secs(60);

/// Records every check and notice, and refuses publishes that touch the label
/// `protected`.
struct Recording {
    checks: Mutex<Vec<ProposedPublish>>,
    published: mpsc::Sender<PublishedBatch>,
}

#[wheat_server::async_trait]
impl PublishHooks for Recording {
    async fn check(&self, publish: &ProposedPublish) -> Result<(), PublishRefusal> {
        self.checks.lock().unwrap().push(publish.clone());
        if publish.labels.iter().any(|label| label.name == "protected") {
            return Err(PublishRefusal::new("label protected may not be published"));
        }
        Ok(())
    }

    async fn published(&self, batch: &PublishedBatch) {
        self.published.send(batch.clone()).unwrap();
    }
}

fn recording() -> (Arc<Recording>, mpsc::Receiver<PublishedBatch>, ServerConfig) {
    let (sender, receiver) = mpsc::channel();
    let hooks = Arc::new(Recording {
        checks: Mutex::default(),
        published: sender,
    });
    let mut server = ServerConfig::new();
    server.publish_hooks = Some(hooks.clone());
    (hooks, receiver, server)
}

#[test]
fn the_hooks_see_a_publish_before_it_joins_and_its_batch_once_durable() {
    let (hooks, notices, server) = recording();
    let (sides, _server) = sides_configured(server);
    let config = &sides.config;
    let before = sides.writer.label_view(config, "main").unwrap();
    sides.writer.change(config, "a.txt");
    sides.writer.ok_json(config, &["publish"]);
    let after = sides.writer.label_view(config, "main").unwrap();
    assert_ne!(before, after);

    let checks = hooks.checks.lock().unwrap().clone();
    let [check] = checks.as_slice() else {
        panic!("expected one check, got {checks:?}");
    };
    assert_eq!(check.repository, "team/repo");
    assert_eq!(check.principal.name, "anonymous");
    assert!(!check.force && !check.force_labels);
    let [main] = check.labels.as_slice() else {
        panic!("expected main to move, got {:?}", check.labels);
    };
    assert_eq!(main.name, "main");
    assert_eq!(main.before.as_ref().unwrap().view.to_string(), before);
    assert_eq!(main.after.as_ref().unwrap().view.to_string(), after);
    assert!(check.received.objects > 0);
    assert!(check.received.content > 0);

    let batch = notices.recv_timeout(NOTICE_WAIT).unwrap();
    assert_eq!(batch.repository, "team/repo");
    assert_eq!(batch.before, check.expected);
    assert_eq!(batch.after.generation, batch.before.generation + 1);
    let [published] = batch.publishes.as_slice() else {
        panic!("expected one publish, got {:?}", batch.publishes);
    };
    assert_eq!(published.publish.proposed, check.proposed);
    assert_eq!(published.publish.labels, check.labels);
    assert!(published.stored.object_bytes() > 0);
    assert!(!published.stored.content.is_empty());
}

#[test]
fn a_publish_the_check_refuses_is_not_published_or_announced() {
    let (hooks, notices, server) = recording();
    let (sides, _server) = sides_configured(server);
    let config = &sides.config;
    sides
        .writer
        .ok_json(config, &["label", "create", "protected"]);
    let refused = sides.writer.fails(config, &["publish"]);
    assert!(
        refused.contains("label protected may not be published"),
        "{refused}"
    );
    assert_eq!(hooks.checks.lock().unwrap().len(), 1);
    sides.reader.ok_json(config, &["fetch"]);
    assert!(sides.reader.label_view(config, "protected").is_none());

    // The next publish is the first announced: the refused one never was.
    sides
        .writer
        .ok_json(config, &["label", "delete", "protected"]);
    sides.writer.change(config, "b.txt");
    sides.writer.ok_json(config, &["publish"]);
    let batch = notices.recv_timeout(NOTICE_WAIT).unwrap();
    let [published] = batch.publishes.as_slice() else {
        panic!("expected one publish, got {:?}", batch.publishes);
    };
    assert!(
        published
            .publish
            .labels
            .iter()
            .all(|label| label.name != "protected")
    );
    assert_eq!(hooks.checks.lock().unwrap().len(), 2);
}
