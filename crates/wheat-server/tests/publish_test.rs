//! Publish over the wire: the client pushes what it expects the server to lack,
//! then the server walks from its head to the proposed envelope, takes what it
//! needs from the push and asks for the rest, one `ObjectsNeeded` per round. Each
//! round takes exactly the objects it asked for, whole or in chunks, and nothing
//! else.

mod support;

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use wheat_core::{
    ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree, ViewId,
    ViewLabelManifestRecord,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    ContentStore, DeltaStep, Durability, ImmutableObjectStore, LocalPublicationStore,
    LooseObjectStore, ObjectStoreError, RepositoryPublisher, RepositoryReader, RepositoryStore,
    SeparateRepositoryStore, StoredObject, StructuralDelta, StructuralDeltaLimits,
};

use support::{
    Upload, Ws, connect_and_handshake, content_batch, digest, edit, edits, object_batch,
    object_chunk, object_id, open_repo, push_end, recv_msg, request_publish,
    request_publish_pushing, send_msg, upload_objects,
};

const REPO: &str = "test/repo";
const OP: u32 = 1;

/// A server holding a one-change repository, and a client holding the same
/// repository that it grows and publishes.
struct Publishing {
    addr: SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    server_objects: LooseObjectStore,
    server_content: ContentStore,
    /// The server's head before any publish.
    base: (u64, RepositoryEnvelopeId),
    client_objects: LooseObjectStore,
    client_content: ContentStore,
    client_heads: LocalPublicationStore,
    repository: Repository,
    main: ViewId,
    _server_dir: tempfile::TempDir,
    _client_dir: tempfile::TempDir,
}

/// Stores that skip syncs: the tests write thousands of objects and blobs one at
/// a time.
fn scratch_store(dir: &tempfile::TempDir) -> LooseObjectStore {
    LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred)
}

fn scratch_content(dir: &tempfile::TempDir) -> ContentStore {
    ContentStore::with_durability(dir.path().join("content"), Durability::Deferred)
}

/// The server's store, whose seal fails: it stands for a store that buffers
/// objects and cannot make them durable.
struct SealFails(LooseObjectStore);

#[async_trait::async_trait]
impl ImmutableObjectStore for SealFails {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.0.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.0.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.0.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Backend("upload refused".into()))
    }
}

/// The server's store, counting the writes that reach it.
struct CountsWrites {
    store: LooseObjectStore,
    writes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ImmutableObjectStore for CountsWrites {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.store.put_if_absent(id, bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.store.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.store.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.store.exists_many(ids).await
    }
}

/// What the server's store does besides storing.
enum Served {
    Plain,
    SealFails,
    CountsWrites(Arc<AtomicUsize>),
}

impl Publishing {
    async fn start() -> Self {
        Self::start_with(Served::Plain).await
    }

    async fn start_with(served: Served) -> Self {
        let server_dir = tempfile::tempdir().unwrap();
        let server_objects = scratch_store(&server_dir);
        let server_content = scratch_content(&server_dir);
        let server_heads = LocalPublicationStore::new(server_dir.path().join("repository.head"));
        let client_dir = tempfile::tempdir().unwrap();
        let client_objects = scratch_store(&client_dir);
        let client_content = scratch_content(&client_dir);
        let client_heads = LocalPublicationStore::new(client_dir.path().join("repository.head"));

        let readme = b"read me";
        server_content.put(readme).unwrap();
        client_content.put(readme).unwrap();
        let mut repository = Repository::new(Tree::new());
        let base_view = repository.base_view().id;
        let (_, outcome) = repository
            .create_change(
                base_view,
                base_view,
                [],
                edit("README", readme),
                ApplicationContext::new(&client_content),
            )
            .unwrap();
        let head = RepositoryPublisher::new(&server_objects, &server_content, &server_heads)
            .initialize(&repository)
            .await
            .unwrap();
        RepositoryPublisher::new(&client_objects, &client_content, &client_heads)
            .prepare(&repository)
            .await
            .unwrap();

        let content = server_content.clone();
        let served: Box<dyn RepositoryStore> = match served {
            Served::Plain => Box::new(SeparateRepositoryStore::new(
                server_objects.clone(),
                content,
            )),
            Served::SealFails => Box::new(SeparateRepositoryStore::new(
                SealFails(server_objects.clone()),
                content,
            )),
            Served::CountsWrites(writes) => Box::new(SeparateRepositoryStore::new(
                CountsWrites {
                    store: server_objects.clone(),
                    writes,
                },
                content,
            )),
        };
        let registry = Registry::single_with_stores(REPO.to_string(), served, server_heads);
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready, ready_rx) = tokio::sync::oneshot::channel();
        let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
        tokio::spawn(async move {
            run_until(any_port, registry, ServerConfig::new(), ready, shutdown_rx)
                .await
                .unwrap();
        });
        Self {
            addr: ready_rx.await.unwrap(),
            _shutdown: shutdown,
            server_objects,
            server_content,
            base: (head.generation, head.envelope),
            client_objects,
            client_content,
            client_heads,
            repository,
            main: outcome.resulting_view,
            _server_dir: server_dir,
            _client_dir: client_dir,
        }
    }

    /// Adds a change writing `files` on the client and records it under a new
    /// envelope, which is returned.
    async fn propose(&mut self, files: &[(String, Vec<u8>)]) -> RepositoryEnvelopeId {
        for (_, value) in files {
            self.client_content.put(value).unwrap();
        }
        let base_view = self.repository.base_view().id;
        let (_, outcome) = self
            .repository
            .create_change(
                base_view,
                self.main,
                [],
                edits(
                    files
                        .iter()
                        .map(|(path, value)| (path.as_str(), value.as_slice())),
                ),
                ApplicationContext::new(&self.client_content),
            )
            .unwrap();
        self.main = outcome.resulting_view;
        let publisher = RepositoryPublisher::new(
            &self.client_objects,
            &self.client_content,
            &self.client_heads,
        );
        let closure = publisher.prepare(&self.repository).await.unwrap();
        let labels = ViewLabelManifestRecord {
            labels: Default::default(),
        };
        publisher
            .prepare_envelope(closure.state.id, &labels)
            .await
            .unwrap()
    }

    /// Opens a session and asks the server to publish `proposed` on its base head.
    async fn request(&self, proposed: RepositoryEnvelopeId) -> Ws {
        let mut ws = connect_and_handshake(self.addr).await;
        assert_eq!(open_repo(&mut ws, REPO).await, self.base);
        request_publish(&mut ws, OP, proposed, self.base, false).await;
        ws
    }

    /// Like [`Self::request`], but leaves the push and its `PublishPushEnd` to the
    /// caller.
    async fn request_pushing(&self, proposed: RepositoryEnvelopeId) -> Ws {
        let mut ws = connect_and_handshake(self.addr).await;
        assert_eq!(open_repo(&mut ws, REPO).await, self.base);
        request_publish_pushing(&mut ws, OP, proposed, self.base, false).await;
        ws
    }

    /// The objects of `proposed` that the server's head does not prove present, in
    /// the order the client walks them.
    async fn difference(&self, proposed: RepositoryEnvelopeId) -> Vec<(ObjectId, Vec<u8>)> {
        let mut delta = StructuralDelta::new(
            &self.client_objects,
            Some(self.base.1),
            proposed,
            StructuralDeltaLimits::default(),
        );
        let mut objects = Vec::new();
        while let Some(step) = delta.next().await.unwrap() {
            match step {
                DeltaStep::Object(object) => {
                    objects.push((object.id, object.canonical_bytes.to_vec()));
                }
                DeltaStep::Missing(ids) => panic!("the client lacks {ids:?}"),
            }
        }
        objects
    }

    /// The client's copy of object `id`.
    async fn object(&self, id: ObjectId) -> Vec<u8> {
        self.client_objects
            .get(id)
            .await
            .unwrap()
            .canonical_bytes
            .to_vec()
    }

    /// The server's head is where it was before the publish.
    async fn assert_head_unchanged(&self) {
        let mut ws = connect_and_handshake(self.addr).await;
        assert_eq!(open_repo(&mut ws, REPO).await, self.base, "head moved");
    }

    /// The server holds every object and blob of `envelope`.
    async fn assert_server_complete(&self, envelope: RepositoryEnvelopeId) {
        let reader = RepositoryReader::new(&self.server_objects, &self.server_content);
        let state = reader.envelope(envelope).await.unwrap().state;
        reader
            .validate_complete(state)
            .await
            .expect("the server should hold all of the published envelope");
    }
}

/// `count` files, each in a directory of its own.
fn files_in_own_directories(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|n| (format!("d{n:05}/f"), format!("file {n}").into_bytes()))
        .collect()
}

/// Answers the content phase, however many rounds it takes, and commits. Returns
/// the result and the blobs the server asked for.
async fn upload_content_and_commit(
    ws: &mut Ws,
    content: &ContentStore,
) -> (PublishResultOutput, Vec<ContentId>) {
    let mut asked = Vec::new();
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ContentNeeded(needed) => {
                assert_eq!(needed.op_id, OP);
                assert!(needed.content_ids.len() <= MAX_WANT_IDS);
                let mut blobs = Vec::new();
                for id in needed.content_ids {
                    let blob = ContentId::from_digest(id.bytes.as_slice().try_into().unwrap());
                    asked.push(blob);
                    blobs.push((blob, content.get(blob).unwrap()));
                }
                // The test blobs are small: one batch holds a round.
                send_msg(ws, &content_batch(OP, blobs)).await;
                send_msg(
                    ws,
                    &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                        message_type: 0x46,
                        op_id: OP,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ContentComplete(complete) => {
                assert_eq!(complete.op_id, OP);
                break;
            }
            other => panic!("expected ContentNeeded or ContentComplete, got {other:?}"),
        }
    }
    send_msg(
        ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id: OP,
        }),
    )
    .await;
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(result) => return (result, asked),
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }
}

fn assert_published(result: &PublishResultOutput, envelope: RepositoryEnvelopeId) {
    assert!(
        matches!(result.status, PublishResultStatus::PublishStatusOk(_)),
        "publish should succeed, got {:?}: {}",
        result.status,
        result.message
    );
    assert_eq!(result.new_envelope.bytes, envelope.as_bytes());
}

/// Takes the server's refusal and returns its message.
async fn expect_refusal(ws: &mut Ws) -> String {
    let result = match recv_msg(ws).await {
        WheatProtocolMessage::PublishResult(result) => result,
        other => panic!("expected PublishResult, got {other:?}"),
    };
    assert_eq!(result.op_id, OP);
    assert!(
        matches!(result.status, PublishResultStatus::PublishStatusRejected(_)),
        "the publish should be refused, got {:?}: {}",
        result.status,
        result.message
    );
    result.message
}

/// Takes the next `ObjectsNeeded` and returns its IDs.
async fn next_round(ws: &mut Ws) -> Vec<ObjectId> {
    match recv_msg(ws).await {
        WheatProtocolMessage::ObjectsNeeded(needed) => {
            assert_eq!(needed.op_id, OP);
            needed.ids.iter().map(object_id).collect()
        }
        other => panic!("expected ObjectsNeeded, got {other:?}"),
    }
}

async fn send_whole(ws: &mut Ws, objects: Vec<(ObjectId, Vec<u8>)>) {
    let entries = objects
        .into_iter()
        .map(|(id, data)| ObjectEntry {
            id: digest(id.as_bytes()),
            data,
        })
        .collect();
    send_msg(ws, &object_batch(OP, entries)).await;
}

/// Pushes `data` as the blob `id`, in a batch of its own.
async fn push_blob(ws: &mut Ws, id: ContentId, data: Vec<u8>) {
    send_msg(ws, &content_batch(OP, [(id, data)])).await;
}

/// Pushes `objects` and `blobs`, then ends the push.
async fn push(ws: &mut Ws, objects: Vec<(ObjectId, Vec<u8>)>, blobs: &[Vec<u8>]) {
    let count = objects.len();
    if count > 0 {
        send_whole(ws, objects).await;
    }
    for blob in blobs {
        push_blob(ws, ContentId::of(blob), blob.clone()).await;
    }
    send_msg(ws, &push_end(OP, count as u32, blobs.len() as u32)).await;
}

/// The values of `files`.
fn values(files: &[(String, Vec<u8>)]) -> Vec<Vec<u8>> {
    files.iter().map(|(_, value)| value.clone()).collect()
}

/// Takes the server's `Error` for a push that breaks the protocol, then the end of
/// the connection.
async fn expect_broken(ws: &mut Ws, violation: &str) {
    match recv_msg(ws).await {
        WheatProtocolMessage::Error(error) => {
            assert!(error.message.contains(violation), "{}", error.message);
        }
        other => panic!("expected Error, got {other:?}"),
    }
    loop {
        match ws.next().await {
            None | Some(Err(_) | Ok(Message::Close(_))) => return,
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(other)) => panic!("expected the connection to end, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_publish_pushed_whole_needs_no_rounds() {
    let mut publishing = Publishing::start().await;
    let files = files_in_own_directories(3);
    let proposed = publishing.propose(&files).await;
    let difference = publishing.difference(proposed).await;
    let mut ws = publishing.request_pushing(proposed).await;
    push(&mut ws, difference, &values(&files)).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    assert!(rounds.is_empty(), "{rounds:?}");
    let (result, asked) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert!(asked.is_empty(), "{asked:?}");
    assert_published(&result, proposed);
    publishing.assert_server_complete(proposed).await;
}

#[tokio::test]
async fn the_server_asks_only_for_what_the_push_left_out() {
    let mut publishing = Publishing::start().await;
    let files = files_in_own_directories(3);
    let proposed = publishing.propose(&files).await;
    let mut difference = publishing.difference(proposed).await;
    let left_out = difference.split_off(difference.len() / 2);
    let pushed: HashSet<ObjectId> = difference.iter().map(|(id, _)| *id).collect();
    let left_out_ids: Vec<ObjectId> = left_out.iter().map(|(id, _)| *id).collect();
    let held = publishing
        .server_objects
        .exists_many(&left_out_ids)
        .await
        .unwrap();
    let mut ws = publishing.request_pushing(proposed).await;
    push(&mut ws, difference, &values(&files[..1])).await;

    let mut asked = HashSet::new();
    loop {
        match recv_msg(&mut ws).await {
            WheatProtocolMessage::ObjectsNeeded(needed) => {
                let mut answer = Vec::new();
                for id in needed.ids.iter().map(object_id) {
                    assert!(!pushed.contains(&id), "{id} was pushed and asked for");
                    asked.insert(id);
                    answer.push((id, publishing.object(id).await));
                }
                send_whole(&mut ws, answer).await;
            }
            WheatProtocolMessage::ObjectsComplete(_) => break,
            other => panic!("expected ObjectsNeeded or ObjectsComplete, got {other:?}"),
        }
    }
    // The walk cannot prove some objects present that the server's store holds
    // anyway; the server does not ask for those.
    let lacked: HashSet<ObjectId> = left_out
        .into_iter()
        .zip(held)
        .filter(|(_, held)| !held)
        .map(|((id, _), _)| id)
        .collect();
    assert_eq!(asked, lacked);

    let (result, mut asked) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    let mut expected: Vec<ContentId> = files[1..]
        .iter()
        .map(|(_, value)| ContentId::of(value))
        .collect();
    asked.sort();
    expected.sort();
    assert_eq!(asked, expected);
    assert_published(&result, proposed);
    publishing.assert_server_complete(proposed).await;
}

#[tokio::test]
async fn pushed_objects_and_blobs_the_publish_does_not_need_are_not_stored() {
    let mut publishing = Publishing::start().await;
    let files = files_in_own_directories(2);
    let proposed = publishing.propose(&files).await;
    let mut difference = publishing.difference(proposed).await;
    let junk = b"an object no tree names".to_vec();
    let junk_id = ObjectId::of_canonical_bytes(&junk);
    difference.push((junk_id, junk));
    let stray = b"a blob no file holds".to_vec();
    let mut blobs = values(&files);
    blobs.push(stray.clone());
    let mut ws = publishing.request_pushing(proposed).await;
    push(&mut ws, difference, &blobs).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    assert!(rounds.is_empty(), "{rounds:?}");
    let (result, asked) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert!(asked.is_empty(), "{asked:?}");
    assert_published(&result, proposed);
    publishing.assert_server_complete(proposed).await;
    assert!(matches!(
        publishing.server_objects.get(junk_id).await,
        Err(ObjectStoreError::Missing(_))
    ));
    assert!(!publishing.server_content.contains(ContentId::of(&stray)));
}

#[tokio::test]
async fn a_pushed_object_that_does_not_match_its_id_is_refused() {
    let mut publishing = Publishing::start().await;
    let files = [("notes.txt".to_owned(), b"notes".to_vec())];
    let proposed = publishing.propose(&files).await;
    let mut difference = publishing.difference(proposed).await;
    difference[0].1.push(0);
    let mut ws = publishing.request_pushing(proposed).await;
    push(&mut ws, difference, &values(&files)).await;

    let refusal = expect_refusal(&mut ws).await;
    assert!(refusal.contains("identity mismatch"), "{refusal}");
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_pushed_blob_that_does_not_match_its_id_is_refused() {
    let mut publishing = Publishing::start().await;
    let notes = b"notes".to_vec();
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), notes.clone())])
        .await;
    let difference = publishing.difference(proposed).await;
    let count = difference.len() as u32;
    let mut ws = publishing.request_pushing(proposed).await;
    send_whole(&mut ws, difference).await;
    push_blob(&mut ws, ContentId::of(&notes), b"not the notes".to_vec()).await;
    send_msg(&mut ws, &push_end(OP, count, 1)).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    assert!(rounds.is_empty(), "{rounds:?}");
    let refusal = expect_refusal(&mut ws).await;
    assert!(refusal.contains("content identity mismatch"), "{refusal}");
    assert!(!publishing.server_content.contains(ContentId::of(&notes)));
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_push_end_that_miscounts_the_push_ends_the_connection() {
    let publishing = Publishing::start().await;
    let mut ws = publishing.request_pushing(publishing.base.1).await;
    send_msg(&mut ws, &push_end(OP, 1, 0)).await;

    expect_broken(&mut ws, "counts differ").await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_pushed_blob_over_the_threshold_ends_the_connection() {
    let publishing = Publishing::start().await;
    let mut ws = publishing.request_pushing(publishing.base.1).await;
    let blob = vec![7; MAX_PUSHED_BLOB_SIZE + 1];
    push_blob(&mut ws, ContentId::of(&blob), blob).await;

    expect_broken(&mut ws, "over MAX_PUSHED_BLOB_SIZE").await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_push_over_the_limit_ends_the_connection() {
    let publishing = Publishing::start().await;
    let mut ws = publishing.request_pushing(publishing.base.1).await;
    let blob = vec![7; MAX_PUSHED_BLOB_SIZE];
    let id = ContentId::of(&blob);
    for _ in 0..=MAX_PUSHED_BYTES / MAX_PUSHED_BLOB_SIZE {
        push_blob(&mut ws, id, blob.clone()).await;
    }

    expect_broken(&mut ws, "more than MAX_PUSHED_BYTES").await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn any_other_message_before_the_push_ends_ends_the_connection() {
    let publishing = Publishing::start().await;
    let mut ws = publishing.request_pushing(publishing.base.1).await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id: OP,
        }),
    )
    .await;

    expect_broken(&mut ws, "only ObjectBatch and ContentBatch").await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_publish_whose_objects_cannot_be_sealed_leaves_the_head_alone() {
    let mut publishing = Publishing::start_with(Served::SealFails).await;
    let proposed = publishing.propose(&files_in_own_directories(3)).await;
    let mut ws = publishing.request(proposed).await;

    upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    let (result, _) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert!(
        matches!(result.status, PublishResultStatus::PublishStatusError(_)),
        "the publish should fail, got {:?}: {}",
        result.status,
        result.message
    );
    assert_eq!(result.new_envelope.bytes, publishing.base.1.as_bytes());
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_publish_larger_than_one_round_leaves_the_server_complete() {
    let mut publishing = Publishing::start().await;
    // Thousands of directories: the root tree names them all at once, so one
    // step of the walk finds more missing objects than a round may ask for.
    let proposed = publishing
        .propose(&files_in_own_directories(MAX_WANT_IDS + 900))
        .await;
    let mut ws = publishing.request(proposed).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    let sent: usize = rounds.iter().sum();
    assert!(sent > MAX_WANT_IDS, "{rounds:?}");
    assert!(rounds.contains(&MAX_WANT_IDS), "{rounds:?}");

    let (result, _) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert_published(&result, proposed);
    publishing.assert_server_complete(proposed).await;
}

/// Each durable write costs several fsyncs, so a publish's objects are staged and
/// stored together once it is accepted, not once per round of the walk.
#[tokio::test]
async fn a_publish_of_several_rounds_stores_its_objects_in_one_write() {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut publishing = Publishing::start_with(Served::CountsWrites(writes.clone())).await;
    let proposed = publishing
        .propose(&[
            ("a/b/c/one.txt".to_owned(), b"one".to_vec()),
            ("d/e/two.txt".to_owned(), b"two".to_vec()),
        ])
        .await;
    let mut ws = publishing.request(proposed).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    assert!(rounds.len() > 1, "{rounds:?}");
    let (result, _) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert_published(&result, proposed);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    publishing.assert_server_complete(proposed).await;
}

#[tokio::test]
async fn a_publish_refused_after_a_round_stores_nothing() {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut publishing = Publishing::start_with(Served::CountsWrites(writes.clone())).await;
    let proposed = publishing
        .propose(&[("a/b/c/one.txt".to_owned(), b"one".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    let first = next_round(&mut ws).await;
    let mut answered = Vec::new();
    for id in &first {
        answered.push((*id, publishing.object(*id).await));
    }
    send_whole(&mut ws, answered).await;
    let second = next_round(&mut ws).await;
    let unasked = first[0];
    assert!(!second.contains(&unasked), "{second:?}");
    send_whole(&mut ws, vec![(unasked, publishing.object(unasked).await)]).await;

    let refusal = expect_refusal(&mut ws).await;
    assert!(refusal.contains("not requested"), "{refusal}");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    for id in first {
        assert!(
            matches!(
                publishing.server_objects.get(id).await,
                Err(ObjectStoreError::Missing(_))
            ),
            "{id} was stored"
        );
    }
    publishing.assert_head_unchanged().await;
}

/// A head is accepted only once all its content is stored, so a publish asks only
/// about content the current head lacks, not about every file of the new head.
#[tokio::test]
async fn content_is_asked_for_only_where_the_head_lacks_it() {
    let mut publishing = Publishing::start().await;
    // Lose the head's one blob behind the server's back: a content phase that
    // looked at every file would ask for it again.
    std::fs::remove_file(publishing.server_content.path(ContentId::of(b"read me"))).unwrap();
    let notes = b"notes".to_vec();
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), notes.clone())])
        .await;
    let mut ws = publishing.request(proposed).await;

    upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    let (result, asked) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert_published(&result, proposed);
    assert_eq!(asked, [ContentId::of(&notes)]);
}

/// Content is staged with the objects and stored only when the publish is accepted.
#[tokio::test]
async fn content_of_a_publish_that_is_never_committed_is_not_stored() {
    let mut publishing = Publishing::start().await;
    let notes = b"never committed".to_vec();
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), notes.clone())])
        .await;
    let mut ws = publishing.request(proposed).await;

    upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Whole).await;
    let WheatProtocolMessage::ContentNeeded(needed) = recv_msg(&mut ws).await else {
        panic!("expected ContentNeeded");
    };
    let blobs = needed.content_ids.iter().map(|id| {
        let blob = ContentId::from_digest(id.bytes.as_slice().try_into().unwrap());
        (blob, publishing.client_content.get(blob).unwrap())
    });
    send_msg(&mut ws, &content_batch(OP, blobs)).await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
            message_type: 0x46,
            op_id: OP,
        }),
    )
    .await;
    assert!(matches!(
        recv_msg(&mut ws).await,
        WheatProtocolMessage::ContentComplete(_)
    ));

    assert!(!publishing.server_content.contains(ContentId::of(&notes)));
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn objects_sent_in_chunks_are_put_back_together() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"chunked".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    let rounds = upload_objects(&mut ws, OP, &publishing.client_objects, Upload::Chunked).await;
    assert!(!rounds.is_empty());

    let (result, _) = upload_content_and_commit(&mut ws, &publishing.client_content).await;
    assert_published(&result, proposed);
    publishing.assert_server_complete(proposed).await;
}

#[tokio::test]
async fn an_object_the_server_did_not_ask_for_is_refused() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"notes".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    let asked = next_round(&mut ws).await;
    let state: ObjectId =
        RepositoryReader::new(&publishing.client_objects, &publishing.client_content)
            .envelope(proposed)
            .await
            .unwrap()
            .state
            .into();
    assert!(!asked.contains(&state), "{asked:?}");
    send_whole(&mut ws, vec![(state, publishing.object(state).await)]).await;

    let refusal = expect_refusal(&mut ws).await;
    assert!(refusal.contains("not requested"), "{refusal}");
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn an_object_that_does_not_match_its_id_is_refused() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"notes".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    let asked = next_round(&mut ws).await;
    let mut data = publishing.object(asked[0]).await;
    data.push(0);
    send_whole(&mut ws, vec![(asked[0], data)]).await;

    let refusal = expect_refusal(&mut ws).await;
    assert!(refusal.contains("identity mismatch"), "{refusal}");
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_chunk_that_skips_ahead_is_refused() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"notes".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    let asked = next_round(&mut ws).await;
    let data = publishing.object(asked[0]).await;
    send_msg(&mut ws, &object_chunk(OP, asked[0], 1, &data[1..], true)).await;

    expect_refusal(&mut ws).await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn chunks_of_two_objects_mixed_together_are_refused() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"notes".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    // Answer rounds properly until one asks for two objects or more.
    let asked = loop {
        let asked = next_round(&mut ws).await;
        if asked.len() >= 2 {
            break asked;
        }
        let object = publishing.object(asked[0]).await;
        send_whole(&mut ws, vec![(asked[0], object)]).await;
    };
    let first = publishing.object(asked[0]).await;
    let second = publishing.object(asked[1]).await;
    send_msg(&mut ws, &object_chunk(OP, asked[0], 0, &first[..1], false)).await;
    send_msg(&mut ws, &object_chunk(OP, asked[1], 0, &second[..1], false)).await;

    expect_refusal(&mut ws).await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn a_proposed_envelope_that_is_not_a_wheat_object_is_refused() {
    let publishing = Publishing::start().await;
    let garbage = b"not a wheat object".to_vec();
    let id = ObjectId::of_canonical_bytes(&garbage);
    let mut ws = publishing
        .request(RepositoryEnvelopeId::from_digest(*id.as_bytes()))
        .await;

    assert_eq!(next_round(&mut ws).await, [id]);
    send_whole(&mut ws, vec![(id, garbage)]).await;

    expect_refusal(&mut ws).await;
    publishing.assert_head_unchanged().await;
}

#[tokio::test]
async fn any_other_message_during_a_round_is_refused() {
    let mut publishing = Publishing::start().await;
    let proposed = publishing
        .propose(&[("notes.txt".to_owned(), b"notes".to_vec())])
        .await;
    let mut ws = publishing.request(proposed).await;

    next_round(&mut ws).await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
            message_type: 0x46,
            op_id: OP,
        }),
    )
    .await;

    let refusal = expect_refusal(&mut ws).await;
    assert!(
        refusal.contains("only ObjectBatch and ObjectChunk"),
        "{refusal}"
    );
    publishing.assert_head_unchanged().await;
}
