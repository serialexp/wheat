//! Group commit: publishes accepted while a batch is being written are made durable
//! together, joined onto that batch.
//!
//! The store's write is held at a gate, so which publishes share a batch does not
//! depend on how the runtime schedules the committer.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::{Notify, Semaphore};
use wheat_core::{ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree};
use wheat_server::commit::{self, CommitFailure};
use wheat_server::registry::RepositoryEntry;
use wheat_store::{
    ContentItem, ContentStore, ImmutableObjectStore, LocalPublicationStore, ObjectStoreError,
    PackedObjectStore, PersistedRepositoryMutator, PublicationStore, RepositoryHead,
    RepositoryPublisher, RepositoryReader, RepositoryStage, RepositoryStore, RepositoryStoreError,
    StoredObject, finalize_mutation_publication, prepare_mutation_publication,
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
    ) -> Result<(), RepositoryStoreError> {
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
    initial: RepositoryHead,
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
    repo.create_change(
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
        entry: Arc::new(RepositoryEntry::new(Box::new(store), heads)),
        initial,
    }
}

type Stage<'a> = RepositoryStage<'a, dyn RepositoryStore>;

/// A publish's stage holding a new change that writes `path` on top of `base`, as
/// a client would upload it, and the envelope it proposes.
async fn propose<'a>(
    server: &'a Server,
    base: RepositoryHead,
    path: &str,
) -> (Stage<'a>, RepositoryEnvelopeId) {
    let stage = Stage::new(&*server.entry.store, 65_536, 64 * 1024 * 1024);
    let value = path.as_bytes().to_vec();
    stage
        .put_content([(ContentId::of(&value), value.clone(), None)])
        .await
        .unwrap();
    let reader = RepositoryReader::new(&stage, stage.content());
    let envelope = reader.envelope(base.envelope).await.unwrap();
    let basis = reader.state(envelope.state).await.unwrap().base_view;
    let mutator = PersistedRepositoryMutator::new(&stage, stage.content(), &server.entry.heads);
    let (prepared, _) = mutator
        .create_on_frontier(base, basis, basis, proposal(path, &value))
        .await
        .unwrap();
    let publication =
        prepare_mutation_publication(&stage, stage.content(), &server.entry.heads, base, prepared)
            .await
            .unwrap();
    (stage, publication.proposed().envelope)
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
    let stage = Stage::new(&*server.entry.store, 65_536, 64 * 1024 * 1024);
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

    let first_accepted =
        commit::accept(&server.entry, &first_stage, initial.envelope, first, false)
            .await
            .unwrap();
    // The first batch is being written: everything accepted now forms the next one.
    server.gate.arrived.notified().await;

    // A publish still receiving does not see the batch's objects as stored: if the
    // batch failed, it would publish without them.
    let receiving = Stage::new(&*server.entry.store, 65_536, 64 * 1024 * 1024);
    assert_eq!(
        receiving.exists_many(&[first.into()]).await.unwrap(),
        [false]
    );

    // Both join onto the first publish, reading it from memory.
    let a_accepted = commit::accept(&server.entry, &a_stage, initial.envelope, a, false)
        .await
        .unwrap();
    let b_accepted = commit::accept(&server.entry, &b_stage, initial.envelope, b, false)
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
    assert_eq!(server.entry.heads.read().unwrap(), a_head);
    let with_a = joined(&server, initial.envelope, first, a).await;
    assert_eq!(
        a_head.envelope,
        joined(&server, initial.envelope, with_a, b).await
    );
    let reader = RepositoryReader::new(&server.objects, &server.entry.content);
    let state = reader.envelope(a_head.envelope).await.unwrap().state;
    reader.validate_complete(state).await.unwrap();
}

#[tokio::test]
async fn a_failed_batch_fails_the_batch_joined_onto_it() {
    let server = server().await;
    let initial = server.initial;
    let (first_stage, first) = propose(&server, initial, "first.txt").await;
    let (a_stage, a) = propose(&server, initial, "a.txt").await;

    server.gate.failures.store(1, Ordering::SeqCst);
    let first_accepted =
        commit::accept(&server.entry, &first_stage, initial.envelope, first, false)
            .await
            .unwrap();
    server.gate.arrived.notified().await;
    let a_accepted = commit::accept(&server.entry, &a_stage, initial.envelope, a, false)
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
    assert_eq!(server.entry.heads.read().unwrap(), initial);

    // The next publish starts again from the durable head.
    let (c_stage, c) = propose(&server, initial, "c.txt").await;
    let c_accepted = commit::accept(&server.entry, &c_stage, initial.envelope, c, false)
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
    let first_accepted =
        commit::accept(&server.entry, &first_stage, initial.envelope, first, false)
            .await
            .unwrap();
    server.gate.arrived.notified().await;
    let a_accepted = commit::accept(&server.entry, &a_stage, initial.envelope, a, false)
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
    assert_eq!(server.entry.heads.read().unwrap(), initial);

    // A new committer takes the next publish, built on the durable head.
    let (c_stage, c) = propose(&server, initial, "c.txt").await;
    let c_accepted = commit::accept(&server.entry, &c_stage, initial.envelope, c, false)
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

    let a_accepted = commit::accept(&server.entry, &a_stage, initial.envelope, a, false)
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
    let mutator = PersistedRepositoryMutator::new(objects, &content, &server.entry.heads);
    let (prepared, _) = mutator
        .create_on_frontier(initial, basis, basis, proposal("other.txt", b"other.txt"))
        .await
        .unwrap();
    let publication =
        prepare_mutation_publication(objects, &content, &server.entry.heads, initial, prepared)
            .await
            .unwrap();
    let other = finalize_mutation_publication(&server.entry.heads, publication).unwrap();

    // One write for the batch, one for the objects joining it with the other head.
    server.gate.permits.add_permits(2);
    let head = a_accepted.durable().await.unwrap();
    assert_eq!(head.generation, other.generation + 1);
    assert_eq!(
        head.envelope,
        joined(&server, initial.envelope, other.envelope, a).await
    );
    assert_eq!(server.entry.heads.read().unwrap(), head);
}
