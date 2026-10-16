//! Fetch over the wire: the server pushes what the client's `since` lacks, then
//! answers the client's wants, one reply each.

mod support;

use std::collections::HashSet;
use std::net::SocketAddr;

use wheat_core::{
    ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree,
    ViewLabelManifestRecord,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    ContentStore, Durability, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    RepositoryPublisher, RepositoryReader, SeparateRepositoryStore,
};

use support::{
    Fetched, Ws, connect_and_handshake, edit, envelope_id, fetch_complete, fetch_objects,
    fetch_objects_keeping, fetch_request, object_id, object_want, open_repo, recv_msg, send_msg,
};

const REPO: &str = "test/repo";

/// A server holding one history of `changes` changes, the last as its head.
struct Served {
    addr: SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    objects: LooseObjectStore,
    content: ContentStore,
    /// The envelope one change before the head. It is in the store, but no head
    /// points at it.
    earlier: RepositoryEnvelopeId,
    head: RepositoryEnvelopeId,
    /// Every object of `earlier` and of `head`, from the in-memory repository.
    earlier_closure: HashSet<ObjectId>,
    head_closure: HashSet<ObjectId>,
    _dir: tempfile::TempDir,
}

/// Writes the repository's objects and an envelope for them without labels.
/// Returns the envelope and every object it reaches.
async fn record(
    publisher: &RepositoryPublisher<'_, LooseObjectStore, LocalPublicationStore>,
    repository: &Repository,
) -> (RepositoryEnvelopeId, HashSet<ObjectId>) {
    let closure = publisher.prepare(repository).await.unwrap();
    let labels = ViewLabelManifestRecord {
        labels: Default::default(),
    };
    let envelope = publisher
        .prepare_envelope(closure.state.id, &labels)
        .await
        .unwrap();
    let mut ids: HashSet<ObjectId> = closure.objects.iter().map(|object| object.id).collect();
    ids.insert(envelope.into());
    ids.insert(labels.id().into());
    (envelope, ids)
}

async fn serve_history(changes: usize) -> Served {
    // A scratch store that skips syncs: the history is written one object at a
    // time, and a synced store would spend minutes on it.
    let dir = tempfile::tempdir().unwrap();
    let objects = scratch_store(&dir);
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);

    let mut repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut main = base;
    let mut earlier = None;
    for step in 1..=changes {
        if step == changes {
            earlier = Some(record(&publisher, &repository).await);
        }
        let value = format!("value {step}");
        content.put(value.as_bytes()).unwrap();
        // Every fourth change branches off the base view, so lineage is wide too.
        let effective = if step % 4 == 0 { base } else { main };
        let (_, outcome) = repository
            .create_change(
                base,
                effective,
                [],
                edit(&format!("dir{}/file{step}", step % 7), value.as_bytes()),
                ApplicationContext::new(&content),
            )
            .unwrap();
        if effective == main {
            main = outcome.resulting_view;
        }
    }
    let (head, head_closure) = record(&publisher, &repository).await;
    let initialized = publisher.initialize(&repository).await.unwrap().envelope;
    assert_eq!(initialized, head);
    let (earlier, earlier_closure) = earlier.unwrap();

    let registry = Registry::single_with_stores(
        REPO.to_string(),
        Box::new(SeparateRepositoryStore::new(
            objects.clone(),
            content.clone(),
        )),
        heads,
    );
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(any_port, registry, ServerConfig::new(), ready, shutdown_rx)
            .await
            .unwrap();
    });
    Served {
        addr: ready_rx.await.unwrap(),
        _shutdown: shutdown,
        objects,
        content,
        earlier,
        head,
        earlier_closure,
        head_closure,
        _dir: dir,
    }
}

impl Served {
    async fn session(&self) -> Ws {
        let mut ws = connect_and_handshake(self.addr).await;
        let (_, head) = open_repo(&mut ws, REPO).await;
        assert_eq!(head, self.head);
        ws
    }

    /// The head's state object, named as if it were an envelope.
    async fn not_an_envelope(&self) -> RepositoryEnvelopeId {
        let state = RepositoryReader::new(&self.objects, &self.content)
            .envelope(self.head)
            .await
            .unwrap()
            .state;
        RepositoryEnvelopeId::from_digest(*state.as_bytes())
    }
}

/// An envelope ID no store here holds.
fn unknown_envelope() -> RepositoryEnvelopeId {
    RepositoryEnvelopeId::from_digest(*ContentId::of(b"elsewhere").as_bytes())
}

fn scratch_store(dir: &tempfile::TempDir) -> LooseObjectStore {
    LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred)
}

/// Every object of `closure` is in `objects`.
async fn assert_complete(objects: &LooseObjectStore, closure: &HashSet<ObjectId>) {
    let ids: Vec<ObjectId> = closure.iter().copied().collect();
    let present = objects.exists_many(&ids).await.unwrap();
    let missing: Vec<_> = ids
        .iter()
        .zip(present)
        .filter_map(|(id, present)| (!present).then_some(id))
        .collect();
    assert!(missing.is_empty(), "the clone lacks {missing:?}");
}

/// Sends a fetch and takes the push without storing it. Returns the pushed IDs
/// in the order they came.
async fn take_push(
    ws: &mut Ws,
    op_id: u32,
    want: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
) -> Vec<ObjectId> {
    send_msg(ws, &fetch_request(op_id, want, since)).await;
    let mut ids = Vec::new();
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ObjectBatch(batch) => {
                assert_eq!(batch.op_id, op_id);
                assert!(batch.objects.len() <= MAX_OBJECT_BATCH_ITEMS);
                let bytes: usize = batch.objects.iter().map(|o| o.data.len()).sum();
                assert!(bytes <= MAX_OBJECT_BATCH_SIZE);
                ids.extend(batch.objects.iter().map(|o| object_id(&o.id)));
            }
            WheatProtocolMessage::FetchPushEnd(end) => {
                assert_eq!(end.op_id, op_id);
                assert_eq!(end.pushed_objects, ids.len() as u64);
                assert_eq!(end.incremental, since.is_some());
                return ids;
            }
            other => panic!("expected the push, got {other:?}"),
        }
    }
}

fn expect_error(message: WheatProtocolMessage, op_id: u32, code: u16) -> ErrorOutput {
    match message {
        WheatProtocolMessage::Error(error) => {
            assert_eq!(
                (error.op_id, error.code),
                (op_id, code),
                "{}",
                error.message
            );
            error
        }
        other => panic!("expected Error {code}, got {other:?}"),
    }
}

#[tokio::test]
async fn a_clone_arrives_whole_in_the_push() {
    let served = serve_history(120).await;
    let mut ws = served.session().await;
    let dir = tempfile::tempdir().unwrap();
    let clone = scratch_store(&dir);

    let fetched = fetch_objects(&mut ws, 1, &clone, served.head, None).await;
    assert!(
        fetched.pushed > 3 * MAX_OBJECT_BATCH_ITEMS as u64,
        "the history should need several batches: {fetched:?}"
    );
    assert_eq!(
        fetched,
        Fetched {
            pushed: served.head_closure.len() as u64,
            ..Fetched::default()
        },
        "the push should be the closure, with nothing left to ask for"
    );
    assert_complete(&clone, &served.head_closure).await;
}

#[tokio::test]
async fn a_fetch_after_one_change_sends_only_what_changed() {
    let served = serve_history(120).await;
    let mut ws = served.session().await;
    let dir = tempfile::tempdir().unwrap();
    let clone = scratch_store(&dir);

    // Any envelope in the store may be fetched, not just the head.
    let first = fetch_objects(&mut ws, 1, &clone, served.earlier, None).await;
    assert_complete(&clone, &served.earlier_closure).await;
    let update = fetch_objects(&mut ws, 2, &clone, served.head, Some(served.earlier)).await;
    assert!(update.incremental);
    assert_eq!((update.pulled, update.wants), (0, 0), "{update:?}");
    let new = served.head_closure.difference(&served.earlier_closure);
    assert!(update.pushed >= new.count() as u64, "{update:?}");
    assert!(
        update.pushed * 10 < first.pushed,
        "one change should push a small part of the history: {update:?} after {first:?}"
    );
    assert_complete(&clone, &served.head_closure).await;
}

#[tokio::test]
async fn pushed_objects_the_client_lost_are_pulled() {
    let served = serve_history(120).await;
    let mut ws = served.session().await;
    let dir = tempfile::tempdir().unwrap();
    let clone = scratch_store(&dir);

    let fetched =
        fetch_objects_keeping(&mut ws, 1, &clone, served.head, None, |n| n % 3 != 0).await;
    assert!(
        fetched.dropped > MAX_OBJECT_BATCH_ITEMS as u64,
        "{fetched:?}"
    );
    assert_eq!(fetched.pulled, fetched.dropped, "{fetched:?}");
    assert!(
        fetched.wants >= 2,
        "a reply holds at most one batch: {fetched:?}"
    );
    assert_complete(&clone, &served.head_closure).await;
}

#[tokio::test]
async fn a_want_gets_one_reply_from_the_front_of_its_list() {
    let served = serve_history(40).await;
    let mut ws = served.session().await;
    let pushed = take_push(&mut ws, 1, served.head, None).await;
    assert!(pushed.len() > 300, "{}", pushed.len());

    let asked = &pushed[..300];
    send_msg(&mut ws, &object_want(1, 0, asked)).await;
    let reply = match recv_msg(&mut ws).await {
        WheatProtocolMessage::ObjectWantReply(reply) => reply,
        other => panic!("expected ObjectWantReply, got {other:?}"),
    };
    assert_eq!((reply.op_id, reply.want_sequence), (1, 0));
    let answered: Vec<ObjectId> = reply.objects.iter().map(|o| object_id(&o.id)).collect();
    assert_eq!(answered, asked[..MAX_OBJECT_BATCH_ITEMS]);
    for object in &reply.objects {
        assert_eq!(
            ObjectId::of_canonical_bytes(&object.data),
            object_id(&object.id)
        );
    }

    // The client asks again for the rest, with the next number.
    let rest = &asked[MAX_OBJECT_BATCH_ITEMS..];
    send_msg(&mut ws, &object_want(1, 1, rest)).await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::ObjectWantReply(reply) => {
            assert_eq!(reply.want_sequence, 1);
            assert_eq!(reply.objects.len(), rest.len());
        }
        other => panic!("expected ObjectWantReply, got {other:?}"),
    }
    send_msg(&mut ws, &fetch_complete(1)).await;

    // The session goes on.
    let again = take_push(&mut ws, 2, served.head, Some(served.head)).await;
    send_msg(&mut ws, &fetch_complete(2)).await;
    assert!(again.len() < pushed.len());
}

#[tokio::test]
async fn an_unknown_wanted_object_ends_the_fetch() {
    let served = serve_history(5).await;
    let mut ws = served.session().await;
    let pushed = take_push(&mut ws, 1, served.head, None).await;

    let unknown = ObjectId::of_canonical_bytes(b"not an object in this store");
    send_msg(&mut ws, &object_want(1, 0, &[pushed[0], unknown])).await;
    let error = expect_error(recv_msg(&mut ws).await, 1, 404);
    assert_eq!(object_id(&error.object_id), unknown);

    // The fetch is over; the next operation works.
    take_push(&mut ws, 2, served.head, None).await;
    send_msg(&mut ws, &fetch_complete(2)).await;
}

#[tokio::test]
async fn a_want_out_of_order_or_out_of_bounds_is_refused() {
    let served = serve_history(5).await;
    let mut ws = served.session().await;
    let pushed = take_push(&mut ws, 1, served.head, None).await;
    send_msg(&mut ws, &object_want(1, 1, &pushed[..1])).await;
    expect_error(recv_msg(&mut ws).await, 1, 400);

    take_push(&mut ws, 2, served.head, None).await;
    send_msg(&mut ws, &object_want(2, 0, &[])).await;
    expect_error(recv_msg(&mut ws).await, 2, 400);

    take_push(&mut ws, 3, served.head, None).await;
    let too_many = vec![pushed[0]; MAX_WANT_IDS + 1];
    send_msg(&mut ws, &object_want(3, 0, &too_many)).await;
    expect_error(recv_msg(&mut ws).await, 3, 400);
}

#[tokio::test]
async fn a_since_the_server_cannot_use_pushes_everything() {
    let served = serve_history(10).await;
    let mut ws = served.session().await;
    let full = take_push(&mut ws, 1, served.head, None).await;
    send_msg(&mut ws, &fetch_complete(1)).await;

    let not_an_envelope = served.not_an_envelope().await;
    for (op_id, since) in [(2, unknown_envelope()), (3, not_an_envelope)] {
        send_msg(&mut ws, &fetch_request(op_id, served.head, Some(since))).await;
        let mut pushed = 0;
        loop {
            match recv_msg(&mut ws).await {
                WheatProtocolMessage::ObjectBatch(batch) => pushed += batch.objects.len(),
                WheatProtocolMessage::FetchPushEnd(end) => {
                    assert!(!end.incremental, "{since} must not count as a since");
                    assert_eq!(end.pushed_objects, pushed as u64);
                    break;
                }
                other => panic!("expected the push, got {other:?}"),
            }
        }
        assert_eq!(pushed, full.len());
        send_msg(&mut ws, &fetch_complete(op_id)).await;
    }
}

#[tokio::test]
async fn a_want_the_server_does_not_have_is_refused() {
    let served = serve_history(3).await;
    let mut ws = served.session().await;
    let not_an_envelope = served.not_an_envelope().await;
    for (op_id, want) in [(1, unknown_envelope()), (2, not_an_envelope)] {
        send_msg(&mut ws, &fetch_request(op_id, want, None)).await;
        let error = expect_error(recv_msg(&mut ws).await, op_id, 404);
        assert_eq!(envelope_id(&error.object_id), want);
    }
}

#[tokio::test]
async fn a_wanted_object_too_large_for_a_batch_comes_in_chunks() {
    let served = serve_history(3).await;
    // A directory holding a quarter million files is a structural object that
    // grows past a batch.
    let directory = wheat_core::DirectoryNode::new((0u32..250_000).map(|i| {
        let file = wheat_core::FileValue {
            content: ContentId::of(&i.to_le_bytes()),
            kind: wheat_core::FileKind::Regular,
        };
        (format!("file-{i}"), wheat_core::DirectoryEntry::File(file))
    }))
    .unwrap();
    let bytes = directory.canonical_bytes();
    assert!(bytes.len() > MAX_OBJECT_BATCH_SIZE);
    let large = ObjectId::of_canonical_bytes(&bytes);
    served.objects.put_if_absent(large, &bytes).await.unwrap();

    let mut ws = served.session().await;
    let pushed = take_push(&mut ws, 1, served.head, None).await;
    let small = pushed[0];

    // First in the list: it comes as chunks, and the reply carries the rest.
    send_msg(&mut ws, &object_want(1, 0, &[large, small])).await;
    let mut assembled = Vec::new();
    let reply = loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ObjectChunk(chunk) => {
                assert_eq!(object_id(&chunk.object_id), large);
                assert_eq!(chunk.offset, assembled.len() as u64);
                assert!(chunk.data.len() <= MAX_OBJECT_CHUNK_SIZE);
                assembled.extend_from_slice(&chunk.data);
                assert_eq!(chunk.is_final, assembled.len() == bytes.len());
            }
            WheatProtocolMessage::ObjectWantReply(reply) => break reply,
            other => panic!("expected chunks and a reply, got {other:?}"),
        }
    };
    assert_eq!(assembled, bytes);
    let answered: Vec<ObjectId> = reply.objects.iter().map(|o| object_id(&o.id)).collect();
    assert_eq!(answered, [small]);

    // Later in the list: it waits for a want of its own.
    send_msg(&mut ws, &object_want(1, 1, &[small, large])).await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::ObjectWantReply(reply) => {
            let answered: Vec<ObjectId> = reply.objects.iter().map(|o| object_id(&o.id)).collect();
            assert_eq!(answered, [small]);
        }
        other => panic!("expected ObjectWantReply, got {other:?}"),
    }
    send_msg(&mut ws, &fetch_complete(1)).await;
}

fn content_fetch(op_id: u32, ids: &[ContentId]) -> WheatProtocolMessage {
    WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
        message_type: 0x30,
        op_id,
        content_ids: ids
            .iter()
            .map(|id| support::digest(id.as_bytes()))
            .collect(),
    })
}

#[tokio::test]
async fn a_blob_the_server_cannot_read_ends_the_content_fetch() {
    let served = serve_history(3).await;
    let present = served.content.put(b"present").unwrap();
    let missing = ContentId::of(b"never stored");
    let mut ws = served.session().await;

    send_msg(&mut ws, &content_fetch(1, &[present, missing])).await;
    let error = loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ContentBatch(batch) => assert_eq!(batch.op_id, 1),
            other => break expect_error(other, 1, 404),
        }
    };
    assert_eq!(error.object_id.bytes, missing.as_bytes());
}

#[tokio::test]
async fn a_large_blob_beside_a_missing_one_still_comes_in_chunks() {
    let served = serve_history(3).await;
    let bytes: Vec<u8> = (0..MAX_CONTENT_CHUNK_SIZE + 1000)
        .map(|i| (i % 251) as u8)
        .collect();
    let large = served.content.put(&bytes).unwrap();
    let missing = ContentId::of(b"never stored");
    let mut ws = served.session().await;

    send_msg(&mut ws, &content_fetch(1, &[large, missing])).await;
    let mut assembled = Vec::new();
    let error = loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ContentChunk(chunk) => {
                assert_eq!(chunk.content_id.bytes, large.as_bytes());
                assert_eq!(chunk.offset, assembled.len() as u64);
                assembled.extend_from_slice(&chunk.data);
                assert_eq!(chunk.is_final, assembled.len() == bytes.len());
            }
            other => break expect_error(other, 1, 404),
        }
    };
    assert_eq!(assembled, bytes);
    assert_eq!(error.object_id.bytes, missing.as_bytes());
}
