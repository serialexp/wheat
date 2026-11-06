//! Server stages: an edit or a merge an embedder builds on the durable head with the
//! store's mutator, then submits with label changes through the group commit.

use crate::support;

use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ChangeId, ChangeMetadata, ChangeProposal, ContentId, Repository,
    RevisionEdits, RevisionMetadata, Tree, ViewId, ViewLabel,
};
use wheat_server::access::Principal;
use wheat_server::commit::CommitFailure;
use wheat_server::naming::ContentNaming;
use wheat_server::network::Network;
use wheat_server::quiesce::Quiesced;
use wheat_server::registry::RepositoryEntry;
use wheat_server::write::{
    ExpectedLabel::{self, Absent, View},
    LabelChange, ServerStage, ServerWrite, WriteFailure, open_stage, write,
};
use wheat_store::{
    LabelMove, LocalPublicationStore, MerkleTreeStore, PackedObjectStore, RepositoryHead,
    RepositoryPublisher, RepositoryReader,
};

use support::edit;

/// Records each naming; refuses them while `fail` is set.
#[derive(Default)]
struct Recording {
    fail: AtomicBool,
    calls: Mutex<Vec<Vec<ContentId>>>,
}

#[async_trait]
impl ContentNaming for Recording {
    fn renew_after(&self) -> Duration {
        Duration::from_secs(3600)
    }

    async fn named(&self, ids: &[ContentId]) -> io::Result<()> {
        self.calls.lock().unwrap().push(ids.to_vec());
        if self.fail.load(Ordering::Relaxed) {
            return Err(io::Error::other("content was reclaimed"));
        }
        Ok(())
    }
}

struct Server {
    _dir: tempfile::TempDir,
    objects: PackedObjectStore,
    entry: Arc<RepositoryEntry>,
    /// The base view; `first` writes `one.txt` on it and `other` writes `three.txt`
    /// on it beside `first`. `main` names `first` and `feature` names `other`.
    base: ViewId,
    first: ViewId,
    other: ViewId,
}

/// A repository with two changes made apart on its base view, labelled, whose network
/// tells `naming` when given.
async fn server(naming: Option<Arc<Recording>>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    content.put(b"one").unwrap();
    content.put(b"three").unwrap();
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let mut create = |path: &str, value: &[u8]| {
        repo.create_change(
            base,
            base,
            [],
            edit(path, value),
            ApplicationContext::new(&content),
        )
        .unwrap()
        .1
        .resulting_view
    };
    let first = create("one.txt", b"one");
    let other = create("three.txt", b"three");
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    let network = match naming {
        Some(naming) => Network::with_naming(Box::new(objects.clone()), naming),
        None => Network::new(Box::new(objects.clone())),
    };
    let server = Server {
        _dir: dir,
        entry: Arc::new(RepositoryEntry::in_network(network, heads)),
        objects,
        base,
        first,
        other,
    };
    write(
        &server.entry,
        server_write(
            0,
            vec![
                change("main", Absent, Some(first)),
                change("feature", Absent, Some(other)),
            ],
        ),
    )
    .await
    .unwrap();
    server
}

fn change(name: &str, expected: ExpectedLabel, new: Option<ViewId>) -> LabelChange {
    LabelChange {
        name: name.to_owned(),
        expected,
        new: new.map(ViewLabel::from),
    }
}

fn server_write(key: u8, labels: Vec<LabelChange>) -> ServerWrite {
    ServerWrite {
        principal: Principal {
            name: "gothab".into(),
        },
        key: [key; 16],
        labels,
        force_labels: false,
    }
}

fn proposal(edits: RevisionEdits) -> ChangeProposal {
    ChangeProposal {
        id: ChangeId::new(),
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

/// Builds, in `stage`, a change writing `path` on `view`, and answers the view after it.
async fn edit_in(
    stage: &ServerStage,
    view: ViewId,
    path: &'static str,
    value: &'static [u8],
) -> ViewId {
    stage
        .build(move |builder| async move {
            builder.content().put(value).unwrap();
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .create_on_frontier(builder.head(), view, view, proposal(edit(path, value)))
                .await
                .unwrap();
            mutator.publish(prepared).await.unwrap();
            outcome.resulting_view
        })
        .await
        .unwrap()
}

/// The labels `head` holds, by name.
async fn labels(server: &Server, head: RepositoryHead) -> Vec<(String, ViewId)> {
    let content = server.objects.content_store();
    let reader = RepositoryReader::new(&server.objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    reader
        .view_label_manifest(envelope.labels)
        .await
        .unwrap()
        .labels
        .into_iter()
        .map(|(name, label)| (name, label.view))
        .collect()
}

/// What `view` holds at each of `paths`.
async fn files(server: &Server, view: ViewId, paths: &[&str]) -> Vec<Option<ContentId>> {
    let content = server.objects.content_store();
    let tree = RepositoryReader::new(&server.objects, &content)
        .view(view)
        .await
        .unwrap()
        .tree;
    MerkleTreeStore::new(&server.objects)
        .lookup_many(tree, paths)
        .await
        .unwrap()
        .into_iter()
        .map(|value| value.map(|value| value.content))
        .collect()
}

#[tokio::test]
async fn an_edit_built_in_a_stage_is_published_with_its_label() {
    let server = server(None).await;
    let stage = open_stage(&server.entry).await.unwrap();
    let base = stage.base();
    assert_eq!(stage.head(), base);
    let edited = edit_in(&stage, server.first, "new.txt", b"new").await;
    assert_ne!(stage.head(), base, "the build moved the tip");
    // Nothing is published before the submit.
    assert_eq!(server.entry.heads.head().await.unwrap(), base);

    let submit = server_write(1, vec![change("main", View(server.first), Some(edited))]);
    let published = stage.submit(submit.clone()).await.unwrap();
    assert_eq!(published.generation, base.generation + 1);
    assert_eq!(
        labels(&server, published).await,
        [
            ("feature".to_owned(), server.other),
            ("main".to_owned(), edited)
        ]
    );
    assert_eq!(
        files(&server, edited, &["one.txt", "new.txt"]).await,
        [Some(ContentId::of(b"one")), Some(ContentId::of(b"new"))]
    );

    // The stage submitted again gets its first answer; another stage built the same
    // way under that key is refused, since its change is another.
    assert_eq!(stage.submit(submit).await.unwrap(), published);
    let again = open_stage(&server.entry).await.unwrap();
    let rebuilt = edit_in(&again, server.first, "new.txt", b"new").await;
    let reused = again
        .submit(server_write(
            1,
            vec![change("main", View(server.first), Some(rebuilt))],
        ))
        .await;
    assert!(
        matches!(
            reused,
            Err(WriteFailure::Commit(CommitFailure::Rejected { .. }))
        ),
        "{reused:?}"
    );
    assert_eq!(server.entry.heads.head().await.unwrap(), published);
}

#[tokio::test]
async fn a_merge_built_in_a_stage_joins_what_was_published_meanwhile() {
    let server = server(None).await;
    let stage = open_stage(&server.entry).await.unwrap();
    let (first, other, base) = (server.first, server.other, server.base);
    let merged = stage
        .build(move |builder| async move {
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .integrate(builder.head(), base, &[first, other])
                .await
                .unwrap();
            mutator.publish(prepared).await.unwrap();
            assert!(outcome.conflicted.is_empty());
            outcome.resulting_view
        })
        .await
        .unwrap();
    // Meanwhile another write moves the head on.
    let meanwhile = write(
        &server.entry,
        server_write(2, vec![change("release", Absent, Some(first))]),
    )
    .await
    .unwrap();
    assert_ne!(meanwhile, stage.base());

    // The label is checked against the head the submit joins onto.
    let refused = stage
        .submit(server_write(
            3,
            vec![change("release", Absent, Some(merged))],
        ))
        .await;
    assert!(
        matches!(&refused, Err(WriteFailure::LabelMoved { labels, .. }) if *labels == [("release".to_owned(), Some(first))]),
        "{refused:?}"
    );
    // Refused before it was accepted, so the stage may be submitted again.
    let published = stage
        .submit(server_write(
            3,
            vec![
                change("main", View(first), Some(merged)),
                change("feature", View(other), None),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(published.generation, meanwhile.generation + 1);
    assert_eq!(
        labels(&server, published).await,
        [("main".to_owned(), merged), ("release".to_owned(), first)]
    );
    assert_eq!(
        files(&server, merged, &["one.txt", "three.txt"]).await,
        [Some(ContentId::of(b"one")), Some(ContentId::of(b"three"))]
    );
}

#[tokio::test]
async fn a_stage_moves_labels_only_in_its_write() {
    let server = server(None).await;
    let stage = open_stage(&server.entry).await.unwrap();
    let first = server.first;
    let edited = stage
        .build(move |builder| async move {
            builder.content().put(b"new").unwrap();
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .create_on_frontier(
                    builder.head(),
                    first,
                    first,
                    proposal(edit("new.txt", b"new")),
                )
                .await
                .unwrap();
            let moved = LabelMove {
                name: "main".into(),
                from: first,
                to: outcome.resulting_view,
            };
            mutator
                .publish_moving_label(prepared, Some(&moved))
                .await
                .unwrap();
            outcome.resulting_view
        })
        .await
        .unwrap();
    let refused = stage
        .submit(server_write(1, vec![change("dev", Absent, Some(edited))]))
        .await;
    assert!(
        matches!(&refused, Err(WriteFailure::Invalid(message)) if message.contains("move labels in the write")),
        "{refused:?}"
    );
    // A build expecting a tip that has moved on is refused by the stage's head.
    let base = stage.base();
    let stale = stage
        .build(move |builder| async move {
            builder
                .mutator()
                .create_on_frontier(base, first, first, proposal(edit("late.txt", b"new")))
                .await
                .map(|_| ())
        })
        .await
        .unwrap();
    assert!(
        matches!(
            stale,
            Err(wheat_store::RepositoryMutationError::Stale { .. })
        ),
        "{stale:?}"
    );
}

#[tokio::test]
async fn a_submit_names_what_its_state_adds() {
    let naming = Arc::new(Recording::default());
    let server = server(Some(Arc::clone(&naming))).await;
    naming.calls.lock().unwrap().clear();
    let stage = open_stage(&server.entry).await.unwrap();
    let edited = edit_in(&stage, server.first, "new.txt", b"new").await;
    let submit = server_write(1, vec![change("main", View(server.first), Some(edited))]);

    naming.fail.store(true, Ordering::Relaxed);
    let refused = stage.submit(submit.clone()).await;
    assert!(
        matches!(&refused, Err(WriteFailure::Commit(CommitFailure::Failed { message, .. })) if message.contains("submit the stage again")),
        "{refused:?}"
    );
    assert_eq!(server.entry.heads.head().await.unwrap(), stage.base());

    naming.fail.store(false, Ordering::Relaxed);
    let published = stage.submit(submit).await.unwrap();
    assert_eq!(
        labels(&server, published).await,
        [
            ("feature".to_owned(), server.other),
            ("main".to_owned(), edited)
        ]
    );
    // Each submit named the one new file; the head already needed the others.
    let calls = naming.calls.lock().unwrap();
    assert_eq!(
        *calls,
        [vec![ContentId::of(b"new")], vec![ContentId::of(b"new")]]
    );
}

#[tokio::test]
async fn a_stage_holds_off_a_quiesce_and_is_refused_by_one() {
    let server = server(None).await;
    let stage = open_stage(&server.entry).await.unwrap();
    let hold = tokio::spawn(Quiesced::hold(Arc::clone(&server.entry)));
    // The quiesce waits for the open stage; it cannot finish while the stage is held.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!hold.is_finished());
    drop(stage);
    let held = tokio::time::timeout(Duration::from_secs(30), hold)
        .await
        .expect("the quiesce ends once the stage is dropped")
        .unwrap();
    let refused = open_stage(&server.entry).await;
    assert!(
        matches!(refused, Err(WriteFailure::Quiesced { .. })),
        "a stage opened while quiesced"
    );
    drop(held);
    open_stage(&server.entry).await.unwrap();
}
