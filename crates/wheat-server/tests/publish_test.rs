//! Publish over the wire: the server walks from its head to the proposed envelope
//! and asks for what it lacks, one `ObjectsNeeded` per round. Each round takes
//! exactly the objects it asked for, whole or in chunks, and nothing else.

mod support;

use std::net::SocketAddr;

use wheat_core::{
    ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree, ViewId,
    ViewLabelManifestRecord,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    ContentStore, Durability, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    RepositoryPublisher, RepositoryReader,
};

use support::{
    Upload, Ws, connect_and_handshake, digest, edit, edits, object_batch, object_chunk, object_id,
    open_repo, recv_msg, request_publish, send_msg, upload_objects,
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

impl Publishing {
    async fn start() -> Self {
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

        let registry = Registry::single_with_stores(
            REPO.to_string(),
            Box::new(server_objects.clone()),
            server_content.clone(),
            server_heads,
        );
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
                for id in needed.content_ids {
                    let blob = ContentId::from_digest(id.bytes.as_slice().try_into().unwrap());
                    asked.push(blob);
                    let data = content.get(blob).unwrap();
                    send_msg(
                        ws,
                        &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                            message_type: 0x44,
                            op_id: OP,
                            content_id: id,
                            data,
                        }),
                    )
                    .await;
                }
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
