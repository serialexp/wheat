//! Group commit: publishes accepted while a batch is being written are made durable
//! together, joined onto that batch.
//!
//! The store's write is held at a gate, so which publishes share a batch does not
//! depend on how the runtime schedules the committer.

use crate::support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{Notify, Semaphore};
use wheat_core::{
    ApplicationContext, ContentId, ObjectId, PublishReceiptRecord, Repository,
    RepositoryEnvelopeId, Tree, VerifiedContent, ViewId, ViewLabelManifestRecord,
};
use wheat_protocol::{Digest256, PublishRequestOutput, Uuid128};
use wheat_server::access::Principal;
use wheat_server::commit::{self, CommitFailure};
use wheat_server::heads::HeadStore;
use wheat_server::hooks::{Announcement, ProposedPublish, PublishHooks, PublishedBatch};
use wheat_server::network::Network;
use wheat_server::quiesce::Quiesced;
use wheat_server::receipt::PublishKey;
use wheat_server::registry::RepositoryEntry;
use wheat_store::{
    ContentItem, ContentStore, ImmutableObjectStore, LocalPublicationStore, ObjectStoreError,
    PackedObjectStore, PersistedRepositoryMutator, PublicationError, PublicationStore,
    RepositoryHead, RepositoryPublisher, RepositoryReader, RepositoryStage, RepositoryStore,
    RepositoryStoreError, StoredObject, finalize_mutation_publication,
    prepare_mutation_publication,
};

use support::edit;

/// Where a [`GatedStore`]'s publication writes wait.
#[derive(Debug)]
struct Gate {
    /// Notified each time a publication write reaches the gate.
    arrived: Notify,
    /// One permit lets one publication write through.
    permits: Semaphore,
    /// Publication writes still to fail.
    failures: AtomicUsize,
    /// Publication writes still to panic.
    panics: AtomicUsize,
}

/// A packed store whose publication writes wait at a gate, and can be made to fail.
#[derive(Debug)]
struct GatedStore {
    inner: PackedObjectStore,
    gate: Arc<Gate>,
}

#[async_trait]
impl ImmutableObjectStore for GatedStore {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.inner.seal().await
    }
}

#[async_trait]
impl RepositoryStore for GatedStore {
    fn content_store(&self) -> ContentStore {
        self.inner.content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<wheat_store::StoredWrite, RepositoryStoreError> {
        self.gate.arrived.notify_one();
        self.gate.permits.acquire().await.unwrap().forget();
        let failing = self
            .gate
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failing {
            return Err(ObjectStoreError::Backend("the disk is full".into()).into());
        }
        let panicking = self
            .gate
            .panics
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        assert!(!panicking, "a bug in the store");
        self.inner.put_publication(objects, content).await
    }
}

struct Server {
    _dir: tempfile::TempDir,
    /// The store beneath the gate, as another process sharing it would write.
    objects: PackedObjectStore,
    gate: Arc<Gate>,
    entry: Arc<RepositoryEntry>,
    /// The head `entry` serves, as another process sharing it would move it.
    heads: LocalPublicationStore,
    initial: RepositoryHead,
    /// The repository's base view, and the view with its one change.
    base: ViewId,
    greeting: ViewId,
}

/// A repository with one change, served through a [`GatedStore`].
async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    content.put(b"hello wheat").unwrap();
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let (_, greeting) = repo
        .create_change(
            basis,
            basis,
            [],
            edit("greeting.txt", b"hello wheat"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    let initial = heads.read().unwrap();

    let gate = Arc::new(Gate {
        arrived: Notify::new(),
        permits: Semaphore::new(0),
        failures: AtomicUsize::new(0),
        panics: AtomicUsize::new(0),
    });
    let store = GatedStore {
        inner: objects.clone(),
        gate: gate.clone(),
    };
    Server {
        _dir: dir,
        objects,
        gate,
        entry: Arc::new(RepositoryEntry::new(Box::new(store), heads.clone())),
        heads,
        initial,
        base: basis,
        greeting: greeting.resulting_view,
    }
}

/// A publish's stage, owning its handle to the store as a connection's does.
type Stage = Arc<RepositoryStage<Network>>;

/// An empty stage over `network`.
fn new_stage(network: &Network) -> Stage {
    Arc::new(RepositoryStage::new(
        network.clone(),
        65_536,
        64 * 1024 * 1024,
    ))
}

/// A publish's stage holding a new change that writes `path` on top of `base`, as
/// a client would upload it, and the envelope it proposes.
async fn propose(
    server: &Server,
    base: RepositoryHead,
    path: &str,
) -> (Stage, RepositoryEnvelopeId) {
    let stage = new_stage(&server.entry.network);
    let value = path.as_bytes().to_vec();
    stage
        .put_content([(VerifiedContent::new(value.clone()), None)])
        .await
        .unwrap();
    let reader = RepositoryReader::new(&stage, stage.content());
    let envelope = reader.envelope(base.envelope).await.unwrap();
    let basis = reader.state(envelope.state).await.unwrap().base_view;
    let mutator = PersistedRepositoryMutator::new(&stage, stage.content(), &server.heads);
    let (prepared, _) = mutator
        .create_on_frontier(base, basis, basis, proposal(path, &value))
        .await
        .unwrap();
    let publication =
        prepare_mutation_publication(&stage, stage.content(), &server.heads, base, prepared)
            .await
            .unwrap();
    (stage, publication.proposed().envelope)
}

/// The key of a publish of `proposed` sent with the client key `seed`.
fn key(seed: u8, proposed: RepositoryEnvelopeId) -> PublishKey {
    forcing_key(seed, proposed, false, false)
}

/// The key of a publish of `proposed` sent with the client key `seed` and the given
/// `force` and `force_labels`.
fn forcing_key(
    seed: u8,
    proposed: RepositoryEnvelopeId,
    force: bool,
    force_labels: bool,
) -> PublishKey {
    let request = PublishRequestOutput {
        message_type: 0x40,
        op_id: 1,
        publish_key: Uuid128 {
            bytes: vec![seed; 16],
        },
        envelope_id: Digest256 {
            bytes: proposed.as_bytes().to_vec(),
        },
        expected_generation: 0,
        expected_envelope: Digest256 { bytes: vec![0; 32] },
        force,
        force_labels,
    };
    PublishKey::new(
        &Principal {
            name: "test".into(),
        },
        &request,
    )
    .unwrap()
}

fn proposal(title: &str, value: &[u8]) -> wheat_core::ChangeProposal {
    let at = wheat_core::Timestamp {
        seconds: 1_000_000,
        offset_minutes: 0,
    };
    let actor = wheat_core::Actor {
        name: "test".into(),
        email: "test@test".into(),
    };
    wheat_core::ChangeProposal {
        id: wheat_core::ChangeId::new(),
        metadata: wheat_core::ChangeMetadata {
            title: title.to_string(),
            description: String::new(),
            created_at: at,
            creator: actor.clone(),
            provenance: None,
        },
        dependencies: std::collections::BTreeSet::new(),
        revision_metadata: wheat_core::RevisionMetadata {
            message: title.to_string(),
            authored_at: at,
            committed_at: at,
            author: actor.clone(),
            committer: actor,
            provenance: None,
        },
        edits: edit(title, value),
    }
}

/// The envelope a server that joined `later` onto `earlier`, both built on `base`,
/// would publish.
async fn joined(
    server: &Server,
    base: RepositoryEnvelopeId,
    earlier: RepositoryEnvelopeId,
    later: RepositoryEnvelopeId,
) -> RepositoryEnvelopeId {
    let stage = new_stage(&server.entry.network);
    RepositoryReader::new(&stage, stage.content())
        .union_envelopes(Some(base), later, earlier)
        .await
        .unwrap()
}

#[tokio::test]
async fn publishes_accepted_while_a_batch_is_written_share_the_next_head_advance() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;
    let (b_stage, b) = propose(&server, initial, "b.txt").await;

    let first_accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    // The first batch is being written: everything accepted now forms the next one.
    server.gate.arrived.notified().await;

    // A publish still receiving does not see the batch's objects as stored: if the
    // batch failed, it would publish without them.
    let receiving = new_stage(&server.entry.network);
    assert_eq!(
        receiving.exists_many(&[first.into()]).await.unwrap(),
        [false]
    );

    // Both join onto the first publish, reading it from memory.
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    let b_accepted = commit::accept(
        &server.entry,
        &b_stage,
        key(3, b),
        initial.envelope,
        b,
        false,
        false,
        None,
    )
    .await
    .unwrap();

    server.gate.permits.add_permits(2);
    let first_head = first_accepted.durable().await.unwrap();
    let a_head = a_accepted.durable().await.unwrap();
    let b_head = b_accepted.durable().await.unwrap();

    assert_eq!(
        first_head,
        RepositoryHead {
            generation: initial.generation + 1,
            envelope: first,
        }
    );
    // One head advance for both.
    assert_eq!(a_head, b_head);
    assert_eq!(a_head.generation, initial.generation + 2);
    assert_eq!(server.heads.read().unwrap(), a_head);
    let with_a = joined(&server, initial.envelope, first, a).await;
    assert_eq!(
        a_head.envelope,
        joined(&server, initial.envelope, with_a, b).await
    );
    let reader = RepositoryReader::new(&server.objects, &server.entry.network.content);
    let state = reader.envelope(a_head.envelope).await.unwrap().state;
    reader.validate_complete(state).await.unwrap();
}

/// A server write accepted while a batch is being written is checked against that
/// batch's head, not the durable one.
#[tokio::test]
async fn a_server_write_is_checked_against_the_tentative_head() {
    use wheat_server::write::{LabelChange, ServerWrite, WriteFailure, write};
    let server = server().await;
    let label = |key: u8, expected: Option<ViewId>, new: Option<ViewId>| ServerWrite {
        principal: Principal {
            name: "gothab".into(),
        },
        key: [key; 16],
        labels: vec![LabelChange {
            name: "a".into(),
            expected,
            new: new.map(wheat_core::ViewLabel::from),
        }],
        force_labels: false,
    };
    let entry = Arc::clone(&server.entry);
    let create =
        tokio::spawn(async move { write(&entry, label(1, None, Some(server.greeting))).await });
    // The first write's batch is being written; the label exists only in it.
    server.gate.arrived.notified().await;
    assert_eq!(server.heads.read().unwrap(), server.initial);
    let refused = write(&server.entry, label(2, None, Some(server.base))).await;
    assert!(
        matches!(&refused, Err(WriteFailure::LabelMoved { labels, .. }) if labels == &[("a".to_owned(), Some(server.greeting))]),
        "{refused:?}"
    );
    let entry = Arc::clone(&server.entry);
    let greeting = server.greeting;
    let delete = tokio::spawn(async move { write(&entry, label(3, Some(greeting), None)).await });
    server.gate.permits.add_permits(2);
    let created = create.await.unwrap().unwrap();
    let deleted = delete.await.unwrap().unwrap();
    assert_eq!(created.generation, server.initial.generation + 1);
    assert_eq!(deleted.generation, server.initial.generation + 2);
    let reader = RepositoryReader::new(&server.objects, &server.entry.network.content);
    let envelope = reader.envelope(deleted.envelope).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    assert!(labels.labels.is_empty());
}

#[tokio::test]
async fn a_failed_batch_fails_the_batch_joined_onto_it() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;

    server.gate.failures.store(1, Ordering::SeqCst);
    let first_accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);

    match first_accepted.durable().await {
        Err(CommitFailure::Failed { head, message }) => {
            assert_eq!(head, initial);
            assert!(
                message.starts_with("storing the published objects failed"),
                "{message}"
            );
        }
        other => panic!("expected the batch to fail, got {other:?}"),
    }
    match a_accepted.durable().await {
        Err(CommitFailure::Stale { head, .. }) => assert_eq!(head, initial),
        other => panic!("expected the joined publish to be stale, got {other:?}"),
    }
    assert_eq!(server.heads.read().unwrap(), initial);

    // The next publish starts again from the durable head.
    let (c_stage, c) = propose(&server, initial, "c.txt").await;
    let c_accepted = commit::accept(
        &server.entry,
        &c_stage,
        key(4, c),
        initial.envelope,
        c,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);
    assert_eq!(
        c_accepted.durable().await.unwrap(),
        RepositoryHead {
            generation: initial.generation + 1,
            envelope: c,
        }
    );
}

#[tokio::test]
async fn a_committer_that_panics_fails_its_publishes_and_the_next_publish_still_commits() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;

    server.gate.panics.store(1, Ordering::SeqCst);
    let first_accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);

    for accepted in [first_accepted, a_accepted] {
        match accepted.durable().await {
            Err(CommitFailure::Internal(message)) => {
                assert!(message.contains("group commit stopped"), "{message}");
            }
            other => panic!("expected the publish to fail, got {other:?}"),
        }
    }
    assert_eq!(server.heads.read().unwrap(), initial);

    // A new committer takes the next publish, built on the durable head.
    let (c_stage, c) = propose(&server, initial, "c.txt").await;
    let c_accepted = commit::accept(
        &server.entry,
        &c_stage,
        key(4, c),
        initial.envelope,
        c,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);
    assert_eq!(
        c_accepted.durable().await.unwrap(),
        RepositoryHead {
            generation: initial.generation + 1,
            envelope: c,
        }
    );
}

#[tokio::test]
async fn a_batch_is_joined_with_a_head_another_process_published_meanwhile() {
    let server = server().await;
    let initial = server.initial;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;

    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;

    // Another process sharing the store publishes while the batch is written.
    let objects = &server.objects;
    let content = objects.content_store();
    content.put(b"other.txt").unwrap();
    let reader = RepositoryReader::new(objects, &content);
    let basis = reader
        .state(reader.envelope(initial.envelope).await.unwrap().state)
        .await
        .unwrap()
        .base_view;
    let mutator = PersistedRepositoryMutator::new(objects, &content, &server.heads);
    let (prepared, _) = mutator
        .create_on_frontier(initial, basis, basis, proposal("other.txt", b"other.txt"))
        .await
        .unwrap();
    let publication =
        prepare_mutation_publication(objects, &content, &server.heads, initial, prepared)
            .await
            .unwrap();
    let other = finalize_mutation_publication(&server.heads, publication).unwrap();

    // One write for the batch, one for the objects joining it with the other head.
    server.gate.permits.add_permits(2);
    let head = a_accepted.durable().await.unwrap();
    assert_eq!(head.generation, other.generation + 1);
    assert_eq!(
        head.envelope,
        joined(&server, initial.envelope, other.envelope, a).await
    );
    assert_eq!(server.heads.read().unwrap(), head);
}

/// A stage with nothing in it, as a publish sent again has before its transfer.
fn empty(server: &Server) -> Stage {
    new_stage(&server.entry.network)
}

/// The answer to a publish sent again with `key`, checked as a connection does
/// before the transfer: against the head it reads first.
async fn previous(
    server: &Server,
    key: &PublishKey,
) -> Result<Option<commit::Accepted>, CommitFailure> {
    let (head, receipt) = server
        .entry
        .heads
        .head_with_receipt(key.key())
        .await
        .unwrap();
    commit::previous(&server.entry, head, receipt.as_ref(), key).await
}

#[tokio::test]
async fn a_publish_sent_again_while_it_is_written_waits_for_it_and_is_published_once() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;

    let first_accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    // `a` waits in the next batch while the first is written.
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();

    // Each is sent again: before its transfer, and when it is accepted.
    let first_again = previous(&server, &key(1, first))
        .await
        .unwrap()
        .expect("the first publish is being written");
    let empty_stage = empty(&server);
    let a_again = commit::accept(
        &server.entry,
        &empty_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(first_again.repeated() && a_again.repeated());
    assert!(!first_accepted.repeated() && !a_accepted.repeated());

    server.gate.permits.add_permits(2);
    let first_head = first_accepted.durable().await.unwrap();
    assert_eq!(first_again.durable().await.unwrap(), first_head);
    let a_head = a_accepted.durable().await.unwrap();
    assert_eq!(a_again.durable().await.unwrap(), a_head);
    // One advance each, not two.
    assert_eq!(a_head.generation, initial.generation + 2);
    assert_eq!(server.heads.read().unwrap(), a_head);
}

#[tokio::test]
async fn a_publish_sent_again_after_it_is_published_gets_its_own_head() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);
    let first_head = accepted.durable().await.unwrap();

    // Another publish moves the head on.
    let (a_stage, a) = propose(&server, first_head, "a.txt").await;
    let accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        first_head.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(1);
    let a_head = accepted.durable().await.unwrap();

    // Sent again, the first is answered with the head it was published in rather
    // than refused as stale, and the head stays.
    let again = previous(&server, &key(1, first))
        .await
        .unwrap()
        .expect("the first publish has a receipt");
    assert!(again.repeated());
    assert_eq!(again.durable().await.unwrap(), first_head);
    let empty_stage = empty(&server);
    let again = commit::accept(
        &server.entry,
        &empty_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(again.durable().await.unwrap(), first_head);
    assert_eq!(server.heads.read().unwrap(), a_head);
}

/// A head kept as a database would keep it: the head in one cell, compared and swapped,
/// and each receipt in a table under its key.
#[derive(Clone)]
struct TableHeads(Arc<Mutex<Table>>);

struct Table {
    head: RepositoryHead,
    receipts: HashMap<[u8; 16], PublishReceiptRecord>,
    advances: usize,
    /// Another host is the repository's primary now: advances are refused.
    fenced: bool,
}

impl TableHeads {
    fn new(head: RepositoryHead) -> Self {
        Self(Arc::new(Mutex::new(Table {
            head,
            receipts: HashMap::new(),
            advances: 0,
            fenced: false,
        })))
    }

    fn set_fenced(&self, fenced: bool) {
        self.0.lock().unwrap().fenced = fenced;
    }

    fn advances(&self) -> usize {
        self.0.lock().unwrap().advances
    }
}

#[async_trait]
impl HeadStore for TableHeads {
    async fn head(&self) -> Result<RepositoryHead, PublicationError> {
        Ok(self.0.lock().unwrap().head)
    }

    async fn head_with_receipt(
        &self,
        key: &[u8; 16],
    ) -> Result<(RepositoryHead, Option<PublishReceiptRecord>), PublicationError> {
        let table = self.0.lock().unwrap();
        Ok((table.head, table.receipts.get(key).copied()))
    }

    async fn advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
        receipts: &[PublishReceiptRecord],
        _now: u64,
    ) -> Result<(), PublicationError> {
        let mut table = self.0.lock().unwrap();
        if table.fenced {
            return Err(PublicationError::Io(std::io::Error::other(
                "this host is no longer the primary",
            )));
        }
        if table.head != expected {
            return Err(PublicationError::Stale {
                expected,
                actual: table.head,
            });
        }
        table.head = proposed;
        for receipt in receipts {
            table.receipts.insert(receipt.key, *receipt);
        }
        table.advances += 1;
        Ok(())
    }
}

#[tokio::test]
async fn a_head_store_of_its_own_serves_publishes_and_answers_them_sent_again() {
    let server = server().await;
    let initial = server.initial;
    let table = TableHeads::new(initial);
    let entry = Arc::new(RepositoryEntry::new(
        Box::new(server.objects.clone()),
        table.clone(),
    ));
    let (stage, proposed) = propose(&server, initial, "first.txt").await;
    let accepted = commit::accept(
        &entry,
        &stage,
        key(1, proposed),
        initial.envelope,
        proposed,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    let head = accepted.durable().await.unwrap();
    assert_eq!(head.generation, initial.generation + 1);
    assert_eq!(table.head().await.unwrap(), head);
    let (_, receipt) = table
        .head_with_receipt(key(1, proposed).key())
        .await
        .unwrap();
    assert_eq!(
        receipt.map(|receipt| receipt.generation),
        Some(head.generation)
    );

    // Sent again, the publish is answered from the table's receipt, and the head is
    // not moved a second time.
    let empty_stage = new_stage(&entry.network);
    let again = commit::accept(
        &entry,
        &empty_stage,
        key(1, proposed),
        initial.envelope,
        proposed,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(again.repeated());
    assert_eq!(again.durable().await.unwrap(), head);
    assert_eq!(table.advances(), 1);
}

#[tokio::test]
async fn a_head_store_that_fences_this_host_out_fails_the_publish_unacknowledged() {
    let server = server().await;
    let initial = server.initial;
    let table = TableHeads::new(initial);
    let entry = Arc::new(RepositoryEntry::new(
        Box::new(server.objects.clone()),
        table.clone(),
    ));
    // The primary moved to another host after this publish was sent.
    table.set_fenced(true);
    let (sender, mut notices) = tokio::sync::mpsc::unbounded_channel();
    let hooks: Arc<dyn PublishHooks> = Arc::new(Announced(sender));
    let (stage, proposed) = propose(&server, initial, "first.txt").await;
    let accepted = commit::accept(
        &entry,
        &stage,
        key(1, proposed),
        initial.envelope,
        proposed,
        false,
        false,
        announcement(&hooks, initial, proposed),
    )
    .await
    .unwrap();
    match accepted.durable().await {
        Err(CommitFailure::Failed { head, message }) => {
            assert_eq!(head, initial);
            assert!(message.contains("no longer the primary"), "{message}");
        }
        other => panic!("expected the advance to fail, got {other:?}"),
    }
    assert_eq!(table.head().await.unwrap(), initial);
    assert_eq!(table.advances(), 0);

    // Nothing was announced: the first batch announced is the next durable one.
    table.set_fenced(false);
    let (stage, next) = propose(&server, initial, "next.txt").await;
    let accepted = commit::accept(
        &entry,
        &stage,
        key(2, next),
        initial.envelope,
        next,
        false,
        false,
        announcement(&hooks, initial, next),
    )
    .await
    .unwrap();
    accepted.durable().await.unwrap();
    let batch = notices.recv().await.unwrap();
    assert_eq!(batch.before, initial);
    let proposed: Vec<_> = batch
        .publishes
        .iter()
        .map(|published| published.publish.proposed)
        .collect();
    assert_eq!(proposed, [next]);
}

#[tokio::test]
async fn a_quiesce_waits_for_the_batch_being_written_after_its_publish_went_away() {
    let server = server().await;
    let initial = server.initial;
    let (stage, proposed) = propose(&server, initial, "first.txt").await;
    let accepted = commit::accept(
        &server.entry,
        &stage,
        key(1, proposed),
        initial.envelope,
        proposed,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    // The batch is at the store's gate, and nobody waits for its answer.
    server.gate.arrived.notified().await;
    drop(accepted);
    drop(stage);

    let quiesce = tokio::spawn(Quiesced::hold(Arc::clone(&server.entry)));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let finished_early = quiesce.is_finished();
    // Let the batch through before asserting, so a failure does not leave the
    // committer at the gate.
    server.gate.permits.add_permits(1);
    let _quiesced = quiesce.await.unwrap();
    assert!(!finished_early, "the committer still wrote");
    assert_eq!(
        server.heads.read().unwrap().generation,
        initial.generation + 1
    );
}

#[tokio::test]
async fn a_publish_key_used_for_a_different_publish_is_refused() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;
    let accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;

    // While the first is written, and once it is published.
    let refused = |answer: Result<Option<commit::Accepted>, CommitFailure>| match answer {
        Err(CommitFailure::Rejected { message, .. }) => {
            assert!(message.contains("publish key was used"), "{message}")
        }
        Err(other) => panic!("expected a refusal, got {other:?}"),
        Ok(other) => panic!(
            "expected a refusal, got repeated={:?}",
            other.map(|a| a.repeated())
        ),
    };
    refused(previous(&server, &key(1, a)).await);
    server.gate.permits.add_permits(1);
    let first_head = accepted.durable().await.unwrap();
    refused(
        commit::accept(
            &server.entry,
            &a_stage,
            key(1, a),
            initial.envelope,
            a,
            false,
            false,
            None,
        )
        .await
        .map(Some),
    );
    assert_eq!(server.heads.read().unwrap(), first_head);
}
fn manifest(labels: &[(&str, ViewId)]) -> ViewLabelManifestRecord {
    ViewLabelManifestRecord::new(labels.iter().copied()).unwrap()
}

/// A publish's stage holding an envelope of `head`'s state with `labels` instead of
/// its own, and that envelope.
async fn relabel(
    server: &Server,
    head: RepositoryHead,
    labels: &[(&str, ViewId)],
) -> (Stage, RepositoryEnvelopeId) {
    let stage = empty(server);
    let state = RepositoryReader::new(&stage, stage.content())
        .envelope(head.envelope)
        .await
        .unwrap()
        .state;
    let envelope = RepositoryPublisher::new(&stage, stage.content(), &server.heads)
        .stage_envelope(state, &manifest(labels))
        .await
        .unwrap();
    (stage, envelope)
}

/// Publishes `head`'s state with `labels` straight to the server's store, as another
/// process sharing it would.
async fn relabel_directly(
    server: &Server,
    head: RepositoryHead,
    labels: &[(&str, ViewId)],
) -> RepositoryHead {
    let content = server.objects.content_store();
    let publisher = RepositoryPublisher::new(&server.objects, &content, &server.heads);
    let state = RepositoryReader::new(&server.objects, &content)
        .envelope(head.envelope)
        .await
        .unwrap()
        .state;
    let envelope = publisher
        .stage_envelope(state, &manifest(labels))
        .await
        .unwrap();
    publisher.publish_envelope(head, envelope).await.unwrap()
}

/// The view label `name` names in the server's head.
async fn label_view(server: &Server, name: &str) -> Option<ViewId> {
    let content = server.objects.content_store();
    let reader = RepositoryReader::new(&server.objects, &content);
    let head = server.heads.read().unwrap();
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    labels.labels.get(name).map(|label| label.view)
}

fn assert_rejected(outcome: Result<commit::Accepted, CommitFailure>, needle: &str) {
    match outcome {
        Err(CommitFailure::Rejected { message, .. }) => {
            assert!(message.contains(needle), "{message}");
        }
        Err(other) => panic!("expected a rejection, got {other:?}"),
        Ok(_) => panic!("expected a rejection, got an accepted publish"),
    }
}

fn assert_stale(outcome: Result<commit::Accepted, CommitFailure>, needle: &str) {
    match outcome {
        Err(CommitFailure::Stale { message, .. }) => {
            assert!(message.contains(needle), "{message}");
        }
        Err(other) => panic!("expected a stale refusal, got {other:?}"),
        Ok(_) => panic!("expected a stale refusal, got an accepted publish"),
    }
}

/// A publish moving `main` from the view with the change to the base view, which
/// lacks it, is refused unless it forces labels, forced or not; forcing the head alone
/// does not move a label backward.
#[tokio::test]
async fn a_backward_label_move_is_refused_unless_the_publish_forces_labels() {
    for (force, force_labels) in [(false, false), (true, false), (false, true), (true, true)] {
        let server = server().await;
        let labeled = relabel_directly(&server, server.initial, &[("main", server.greeting)]).await;
        let (stage, backward) = relabel(&server, labeled, &[("main", server.base)]).await;
        server.gate.permits.add_permits(1);
        let outcome = commit::accept(
            &server.entry,
            &stage,
            forcing_key(1, backward, force, force_labels),
            labeled.envelope,
            backward,
            force,
            force_labels,
            None,
        )
        .await;
        if force_labels {
            let head = outcome.unwrap().durable().await.unwrap();
            assert_eq!(head.envelope, backward, "force={force}");
            assert_eq!(label_view(&server, "main").await, Some(server.base));
        } else {
            assert_rejected(outcome, "label main would move");
            assert_eq!(server.heads.read().unwrap(), labeled, "force={force}");
        }
    }
}

/// A forward move needs no flag, and neither do created or deleted labels.
#[tokio::test]
async fn forward_moves_and_new_or_deleted_labels_are_accepted() {
    let server = server().await;
    let labeled = relabel_directly(
        &server,
        server.initial,
        &[("main", server.base), ("old", server.base)],
    )
    .await;
    let (stage, forward) = relabel(
        &server,
        labeled,
        &[("main", server.greeting), ("new", server.base)],
    )
    .await;
    server.gate.permits.add_permits(1);
    let head = commit::accept(
        &server.entry,
        &stage,
        key(1, forward),
        labeled.envelope,
        forward,
        false,
        false,
        None,
    )
    .await
    .unwrap()
    .durable()
    .await
    .unwrap();
    assert_eq!(head.envelope, forward);
    assert_eq!(label_view(&server, "main").await, Some(server.greeting));
}

/// The permission `force_labels` gives belongs to its publish: one joined after it in
/// the same batch that moves no label is accepted, and one that moves another label
/// backward without the flag is refused.
#[tokio::test]
async fn forcing_labels_permits_only_its_own_publish() {
    let server = server().await;
    let labeled = relabel_directly(
        &server,
        server.initial,
        &[("main", server.greeting), ("other", server.greeting)],
    )
    .await;
    let (forced_stage, forced) = relabel(
        &server,
        labeled,
        &[("main", server.base), ("other", server.greeting)],
    )
    .await;
    let first = commit::accept(
        &server.entry,
        &forced_stage,
        forcing_key(1, forced, false, true),
        labeled.envelope,
        forced,
        false,
        true,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;

    // Built on the forced move, it moves `other` backward too, without the flag.
    let (back_stage, back) = relabel(
        &server,
        labeled,
        &[("main", server.base), ("other", server.base)],
    )
    .await;
    assert_rejected(
        commit::accept(
            &server.entry,
            &back_stage,
            key(2, back),
            forced,
            back,
            false,
            false,
            None,
        )
        .await,
        "label other would move",
    );

    // Built on the head before the forced move, it leaves the labels alone, so it
    // joins onto the forced move without moving `main` again.
    let (a_stage, a) = propose(&server, labeled, "a.txt").await;
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(3, a),
        labeled.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();

    server.gate.permits.add_permits(2);
    first.durable().await.unwrap();
    let head = a_accepted.durable().await.unwrap();
    assert_eq!(
        head.envelope,
        joined(&server, labeled.envelope, forced, a).await
    );
    assert_eq!(label_view(&server, "main").await, Some(server.base));
    assert_eq!(label_view(&server, "other").await, Some(server.greeting));
}

/// A label the publish moves and the head moved differently since the publish's base
/// refuses the publish as stale: here the first publish moves `main` forward, and the
/// second, built on the same head, deletes it.
#[tokio::test]
async fn a_label_moved_differently_by_an_accepted_publish_refuses_the_publish_as_stale() {
    let server = server().await;
    let labeled = relabel_directly(&server, server.initial, &[("main", server.base)]).await;
    let (moving_stage, moving) = relabel(&server, labeled, &[("main", server.greeting)]).await;
    let (deleting_stage, deleting) = relabel(&server, labeled, &[]).await;
    let moved = commit::accept(
        &server.entry,
        &moving_stage,
        key(1, moving),
        labeled.envelope,
        moving,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert_stale(
        commit::accept(
            &server.entry,
            &deleting_stage,
            key(2, deleting),
            labeled.envelope,
            deleting,
            false,
            false,
            None,
        )
        .await,
        "labels were moved differently",
    );
    server.gate.permits.add_permits(1);
    moved.durable().await.unwrap();
}

/// As above when the batch is re-joined: another process sharing the store deletes
/// `main` while the batch moving it is written.
#[tokio::test]
async fn a_label_moved_differently_by_another_process_refuses_the_batch_as_stale() {
    let server = server().await;
    let labeled = relabel_directly(&server, server.initial, &[("main", server.base)]).await;
    let (moving_stage, moving) = relabel(&server, labeled, &[("main", server.greeting)]).await;
    let moved = commit::accept(
        &server.entry,
        &moving_stage,
        key(1, moving),
        labeled.envelope,
        moving,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    let deleted = relabel_directly(&server, labeled, &[]).await;
    server.gate.permits.add_permits(1);
    match moved.durable().await {
        Err(CommitFailure::Stale { head, message }) => {
            assert_eq!(head, deleted);
            assert!(
                message.contains("labels were moved differently"),
                "{message}"
            );
        }
        other => panic!("expected a stale refusal, got {other:?}"),
    }
    assert_eq!(server.heads.read().unwrap(), deleted);
}

/// `force_labels` is part of what a publish key names: the same key sent again with a
/// different flag is a different publish.
#[tokio::test]
async fn a_publish_key_reused_with_a_different_force_labels_is_refused() {
    let server = server().await;
    let (stage, a) = propose(&server, server.initial, "a.txt").await;
    let accepted = commit::accept(
        &server.entry,
        &stage,
        key(1, a),
        server.initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    match previous(&server, &forcing_key(1, a, false, true)).await {
        Err(CommitFailure::Rejected { message, .. }) => {
            assert!(message.contains("publish key was used"), "{message}");
        }
        Err(other) => panic!("expected a refusal, got {other:?}"),
        Ok(other) => panic!(
            "expected a refusal, got repeated={:?}",
            other.map(|a| a.repeated())
        ),
    }
    server.gate.permits.add_permits(1);
    accepted.durable().await.unwrap();
}

/// Sends each durable batch it is told about.
struct Announced(tokio::sync::mpsc::UnboundedSender<PublishedBatch>);

#[async_trait]
impl PublishHooks for Announced {
    async fn published(&self, batch: &PublishedBatch) {
        let _ = self.0.send(batch.clone());
    }
}

/// An announcement of a publish of `proposed` built on `expected`, to `hooks`.
fn announcement(
    hooks: &Arc<dyn PublishHooks>,
    expected: RepositoryHead,
    proposed: RepositoryEnvelopeId,
) -> Option<Announcement> {
    Some(Announcement {
        hooks: Arc::clone(hooks),
        publish: ProposedPublish {
            repository: "test/repo".into(),
            principal: Principal {
                name: "test".into(),
            },
            expected,
            proposed,
            force: false,
            force_labels: false,
            labels: Vec::new(),
            received: Default::default(),
        },
    })
}

#[tokio::test]
async fn each_durable_batch_is_announced_with_what_each_publish_stored() {
    let server = server().await;
    let initial = server.initial;
    let (sender, mut notices) = tokio::sync::mpsc::unbounded_channel();
    let hooks: Arc<dyn PublishHooks> = Arc::new(Announced(sender));
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;
    let (b_stage, b) = propose(&server, initial, "b.txt").await;

    let first_accepted = commit::accept(
        &server.entry,
        &first_stage,
        key(1, first),
        initial.envelope,
        first,
        false,
        false,
        announcement(&hooks, initial, first),
    )
    .await
    .unwrap();
    server.gate.arrived.notified().await;
    // Both join the next batch; only the first of them is announced.
    let a_accepted = commit::accept(
        &server.entry,
        &a_stage,
        key(2, a),
        initial.envelope,
        a,
        false,
        false,
        None,
    )
    .await
    .unwrap();
    let b_accepted = commit::accept(
        &server.entry,
        &b_stage,
        key(3, b),
        initial.envelope,
        b,
        false,
        false,
        announcement(&hooks, initial, b),
    )
    .await
    .unwrap();
    server.gate.permits.add_permits(2);
    let first_head = first_accepted.durable().await.unwrap();
    let a_head = a_accepted.durable().await.unwrap();
    assert_eq!(b_accepted.durable().await.unwrap(), a_head);

    let wait = std::time::Duration::from_secs(60);
    let stored = |batch: &PublishedBatch, index: usize, path: &str| {
        batch.publishes[index]
            .stored
            .content
            .iter()
            .any(|(id, _)| *id == ContentId::of(path.as_bytes()))
    };
    let batch = tokio::time::timeout(wait, notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((batch.before, batch.after), (initial, first_head));
    assert_eq!(batch.repository, "test/repo");
    assert_eq!(batch.publishes.len(), 1);
    assert_eq!(batch.publishes[0].publish.proposed, first);
    assert!(stored(&batch, 0, "first.txt"));

    let batch = tokio::time::timeout(wait, notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((batch.before, batch.after), (first_head, a_head));
    assert_eq!(batch.publishes.len(), 1);
    assert_eq!(batch.publishes[0].publish.proposed, b);
    // What b stored, not what a, the publish before it in the batch, did.
    assert!(stored(&batch, 0, "b.txt"));
    assert!(!stored(&batch, 0, "a.txt"));
}
