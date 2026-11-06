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
use wheat_core::{PublishReceiptRecord, RepositoryEnvelopeId};
use wheat_server::access::Principal;
use wheat_server::commit::CommitFailure;
use wheat_server::heads::{HeadStore, ReceiptLookup, conformance::check_head_store};
use wheat_server::naming::ContentNaming;
use wheat_server::network::Network;
use wheat_server::quiesce::Quiesced;
use wheat_server::registry::RepositoryEntry;
use wheat_server::write::{
    ExpectedLabel::{self, Absent, View},
    LabelChange, ServerStage, ServerWrite, Submission, WriteFailure, open_stage, published, write,
};
use wheat_store::{
    LabelMove, LocalPublicationStore, MerkleTreeStore, PackedObjectStore, PublicationError,
    PublicationStore, RepositoryHead, RepositoryPublisher, RepositoryReader,
};

use support::edit;

/// What the tests' submits ask for.
const INTENT: [u8; 32] = [7; 32];

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
    server_in(naming, None).await
}

/// [`server`], keeping its head in a [`CappedHeads`] holding `limit` receipts when
/// given.
async fn server_in(naming: Option<Arc<Recording>>, limit: Option<usize>) -> Server {
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
    let entry = match limit {
        None => RepositoryEntry::in_network(network, heads),
        Some(limit) => {
            let initial = PublicationStore::read(&heads).unwrap();
            RepositoryEntry::in_network(network, CappedHeads::new(initial, limit))
        }
    };
    let server = Server {
        _dir: dir,
        entry: Arc::new(entry),
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

/// A head store keeping at most `limit` receipts, dropping the oldest past it however
/// young, as the local one does past `MAX_PUBLISH_RECEIPTS`.
struct CappedHeads {
    limit: usize,
    /// The limit it declares, which is `limit` unless a test has it lie.
    declared: Option<usize>,
    table: Mutex<(RepositoryHead, Vec<PublishReceiptRecord>)>,
}

impl CappedHeads {
    fn new(head: RepositoryHead, limit: usize) -> Self {
        Self {
            limit,
            declared: Some(limit),
            table: Mutex::new((head, Vec::new())),
        }
    }
}

#[async_trait]
impl HeadStore for CappedHeads {
    fn receipt_limit(&self) -> Option<usize> {
        self.declared
    }

    async fn head(&self) -> Result<RepositoryHead, PublicationError> {
        Ok(self.table.lock().unwrap().0)
    }

    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, ReceiptLookup), PublicationError> {
        let table = self.table.lock().unwrap();
        let (head, receipts) = &*table;
        let lookup = match receipts.iter().find(|receipt| &receipt.key == key) {
            Some(receipt) => ReceiptLookup::Found(*receipt),
            None if self.declared.is_some() && receipts.len() >= self.limit => {
                ReceiptLookup::Trimmed {
                    oldest_kept: receipts[0].recorded_at,
                }
            }
            None => ReceiptLookup::Absent,
        };
        Ok((*head, lookup))
    }

    async fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        _now: u64,
    ) -> Result<(), PublicationError> {
        let mut table = self.table.lock().unwrap();
        if table.0 != expected {
            return Err(PublicationError::Stale {
                expected,
                actual: table.0,
            });
        }
        table.0 = proposed;
        table.1.extend_from_slice(receipts);
        let over = table.1.len().saturating_sub(self.limit);
        table.1.drain(..over);
        Ok(())
    }
}

#[tokio::test]
async fn the_capped_head_store_keeps_receipts_as_it_promises() {
    let initial = RepositoryHead::initial(RepositoryEnvelopeId::from_digest([1; 32]));
    check_head_store(&CappedHeads::new(initial, 3)).await;
}

#[tokio::test]
#[should_panic(
    expected = "a store keeps every young receipt while it holds no more than its limit (None)"
)]
async fn a_store_that_caps_receipts_it_says_it_keeps_fails_the_check() {
    let initial = RepositoryHead::initial(RepositoryEnvelopeId::from_digest([1; 32]));
    check_head_store(&CappedHeads {
        declared: None,
        ..CappedHeads::new(initial, wheat_core::MAX_PUBLISH_RECEIPTS)
    })
    .await;
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
    let published = stage.submit(submit.clone(), INTENT).await.unwrap();
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

    // The stage submitted again gets its first answer.
    assert_eq!(stage.submit(submit, INTENT).await.unwrap(), published);
}

#[tokio::test]
async fn a_stage_rebuilt_for_the_same_intent_gets_the_first_answer() {
    let server = server(None).await;
    let principal = server_write(1, Vec::new()).principal;
    assert_eq!(
        published(&server.entry, &principal, [1; 16], INTENT)
            .await
            .unwrap(),
        Submission::NotPublished
    );
    let stage = open_stage(&server.entry).await.unwrap();
    let edited = edit_in(&stage, server.first, "new.txt", b"new").await;
    let first = stage
        .submit(
            server_write(1, vec![change("main", View(server.first), Some(edited))]),
            INTENT,
        )
        .await
        .unwrap();
    drop(stage);

    // As after a crash: the answer is found by key and intent before building again.
    assert_eq!(
        published(&server.entry, &principal, [1; 16], INTENT)
            .await
            .unwrap(),
        Submission::Published(first)
    );
    // And a stage built again, whose change is another, gets the first answer too,
    // though the label it names has moved.
    let again = open_stage(&server.entry).await.unwrap();
    let rebuilt = edit_in(&again, server.first, "new.txt", b"new").await;
    assert_ne!(rebuilt, edited);
    let rebuilt_write = server_write(1, vec![change("main", View(server.first), Some(rebuilt))]);
    assert_eq!(
        again.submit(rebuilt_write.clone(), INTENT).await.unwrap(),
        first
    );
    // The key with another intent is refused, by the lookup and by a submit.
    let other = [8; 32];
    for refused in [
        published(&server.entry, &principal, [1; 16], other).await,
        again
            .submit(rebuilt_write, other)
            .await
            .map(Submission::Published),
    ] {
        assert!(
            matches!(
                refused,
                Err(WriteFailure::Commit(CommitFailure::Rejected { .. }))
            ),
            "{refused:?}"
        );
    }
    assert_eq!(server.entry.heads.head().await.unwrap(), first);
}

#[tokio::test]
async fn a_submit_whose_receipt_a_capped_store_dropped_is_unknown() {
    // The store keeps one receipt.
    let server = server_in(None, Some(1)).await;
    let principal = server_write(1, Vec::new()).principal;
    let submit = async |key: u8, path: &'static str, from: ViewId| {
        let stage = open_stage(&server.entry).await.unwrap();
        let edited = edit_in(&stage, from, path, b"new").await;
        stage
            .submit(
                server_write(key, vec![change("main", View(from), Some(edited))]),
                INTENT,
            )
            .await
            .unwrap();
        edited
    };
    let first = submit(1, "first.txt", server.first).await;
    let head = server.entry.heads.head().await.unwrap();
    assert_eq!(
        published(&server.entry, &principal, [1; 16], INTENT)
            .await
            .unwrap(),
        Submission::Published(head)
    );
    // A second submit's receipt takes its place while the first's is young.
    submit(2, "second.txt", first).await;
    for key in [[1; 16], [9; 16]] {
        assert_eq!(
            published(&server.entry, &principal, key, INTENT)
                .await
                .unwrap(),
            Submission::Unknown,
            "{key:?}"
        );
    }
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
        .submit(
            server_write(3, vec![change("release", Absent, Some(merged))]),
            INTENT,
        )
        .await;
    assert!(
        matches!(&refused, Err(WriteFailure::LabelMoved { labels, .. }) if *labels == [("release".to_owned(), Some(first))]),
        "{refused:?}"
    );
    // Refused before it was accepted, so the stage may be submitted again.
    let published = stage
        .submit(
            server_write(
                3,
                vec![
                    change("main", View(first), Some(merged)),
                    change("feature", View(other), None),
                ],
            ),
            INTENT,
        )
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
        .submit(
            server_write(1, vec![change("dev", Absent, Some(edited))]),
            INTENT,
        )
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
    let refused = stage.submit(submit.clone(), INTENT).await;
    assert!(
        matches!(&refused, Err(WriteFailure::Commit(CommitFailure::Failed { message, .. })) if message.contains("submit the stage again")),
        "{refused:?}"
    );
    assert_eq!(server.entry.heads.head().await.unwrap(), stage.base());

    naming.fail.store(false, Ordering::Relaxed);
    let published = stage.submit(submit, INTENT).await.unwrap();
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
