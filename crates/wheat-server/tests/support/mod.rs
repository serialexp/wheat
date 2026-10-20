//! A small protocol client shared by the server's integration tests.
//!
//! The real client lives in `wheat-cli`, a binary crate the tests can't depend on,
//! so this is a second copy of its fetch half. It follows the same rules: take the
//! server's push, then run [`StructuralDelta`] over the local store and pull what
//! is still missing.
//!
//! Each test binary uses a different part of this module.
#![allow(dead_code)]

use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{
    ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue, ObjectId,
    RepositoryEnvelopeId, RevisionEdits,
};
use wheat_protocol::*;
use wheat_store::{
    ContentStore, DeltaStep, ImmutableObjectStore, RepositoryReader, StructuralDelta,
    StructuralDeltaLimits,
};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One edit that writes `value` to `path`.
pub fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    edits([(path, value)])
}

/// Edits that write each value to its path.
pub fn edits<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> RevisionEdits {
    let mut revision = RevisionEdits {
        edits: Default::default(),
        transition: EditTransition::default(),
    };
    for (path, value) in files {
        let record = EditRecord::new(
            path,
            EditOperation::ExactFile {
                expected: None,
                replacement: Some(FileValue {
                    content: ContentId::of(value),
                    kind: FileKind::Regular,
                }),
            },
        )
        .unwrap();
        let id = record.id();
        revision.transition.added.insert(id);
        revision.edits.insert(id, record);
    }
    revision
}

pub async fn send_msg(ws: &mut Ws, msg: &WheatProtocolMessage) {
    let bytes = encode_message(msg).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

pub async fn recv_msg(ws: &mut Ws) -> WheatProtocolMessage {
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        match msg {
            Message::Binary(bytes) => return decode_message(&bytes).unwrap(),
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await.unwrap();
            }
            Message::Pong(_) | Message::Frame(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

/// Connects without a token and waits for the session to be set up.
pub async fn connect_and_handshake(addr: SocketAddr) -> Ws {
    let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();

    let server_hello = recv_msg(&mut ws).await;
    assert!(
        matches!(server_hello, WheatProtocolMessage::ServerHello(_)),
        "expected ServerHello, got {server_hello:?}"
    );
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ClientHello(ClientHelloOutput {
            message_type: 0x02,
            selected_version: 1,
            auth_token: vec![],
            receive_window_bytes: 16 * 1024 * 1024,
            receive_window_messages: 256,
        }),
    )
    .await;
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::SessionEstablished(se) => assert!(matches!(
            se.status,
            SessionEstablishedStatus::SessionStatusOk(_)
        )),
        other => panic!("expected SessionEstablished, got {other:?}"),
    }
    ws
}

/// Opens `name`. Returns the server's head generation and envelope.
pub async fn open_repo(ws: &mut Ws, name: &str) -> (u64, RepositoryEnvelopeId) {
    send_msg(
        ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: name.to_string(),
        }),
    )
    .await;
    match recv_msg(ws).await {
        WheatProtocolMessage::RepositoryState(rs) => {
            (rs.head_generation, envelope_id(&rs.head_envelope))
        }
        other => panic!("expected RepositoryState, got {other:?}"),
    }
}

pub fn digest(bytes: &[u8; 32]) -> Digest256 {
    Digest256 {
        bytes: bytes.to_vec(),
    }
}

pub fn object_id(d: &Digest256) -> ObjectId {
    ObjectId::from_digest(d.bytes.as_slice().try_into().unwrap())
}

pub fn envelope_id(d: &Digest256) -> RepositoryEnvelopeId {
    RepositoryEnvelopeId::from_digest(d.bytes.as_slice().try_into().unwrap())
}

pub fn fetch_request(
    op_id: u32,
    want: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
) -> WheatProtocolMessage {
    WheatProtocolMessage::FetchRequest(FetchRequestOutput {
        message_type: 0x20,
        op_id,
        want_envelope: digest(want.as_bytes()),
        since_envelope: digest(since.as_ref().map_or(&[0; 32], |since| since.as_bytes())),
    })
}

pub fn object_want(op_id: u32, want_sequence: u32, ids: &[ObjectId]) -> WheatProtocolMessage {
    WheatProtocolMessage::ObjectWant(ObjectWantOutput {
        message_type: 0x22,
        op_id,
        want_sequence,
        ids: ids.iter().map(|id| digest(id.as_bytes())).collect(),
    })
}

pub fn fetch_complete(op_id: u32) -> WheatProtocolMessage {
    WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
        message_type: 0x23,
        op_id,
    })
}

/// What one fetch transferred.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Fetched {
    pub pushed: u64,
    pub pulled: u64,
    pub wants: u32,
    pub incremental: bool,
    /// Pushed objects [`fetch_objects_keeping`] threw away.
    pub dropped: u64,
    /// Objects that arrived as chunks.
    pub chunked: u64,
}

/// Fetches the closure of `want` into `objects`.
pub async fn fetch_objects<O: ImmutableObjectStore>(
    ws: &mut Ws,
    op_id: u32,
    objects: &O,
    want: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
) -> Fetched {
    fetch_objects_keeping(ws, op_id, objects, want, since, |_| true).await
}

/// Like [`fetch_objects`], but stores the `n`th pushed object only when
/// `keep(n)` says so, to act out a push that lost objects.
pub async fn fetch_objects_keeping<O: ImmutableObjectStore>(
    ws: &mut Ws,
    op_id: u32,
    objects: &O,
    want: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
    mut keep: impl FnMut(u64) -> bool,
) -> Fetched {
    send_msg(ws, &fetch_request(op_id, want, since)).await;

    let mut fetched = Fetched::default();
    let mut receiver = TransferReceiver::unrequested(MAX_OBJECT_SIZE);
    let mut arrived = Vec::new();
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ObjectBatch(batch) => {
                assert_eq!(batch.op_id, op_id);
                for entry in batch.objects {
                    let id = object_id(&entry.id);
                    receiver.accept_whole(id, entry.data.len()).unwrap();
                    arrived.push((id, entry.data));
                }
            }
            WheatProtocolMessage::ObjectChunk(chunk) => {
                assert_eq!(chunk.op_id, op_id);
                let id = object_id(&chunk.object_id);
                if let Some(object) = receiver
                    .accept_chunk(id, chunk.offset, chunk.data, chunk.is_final)
                    .unwrap()
                {
                    fetched.chunked += 1;
                    arrived.push(object);
                }
            }
            WheatProtocolMessage::FetchPushEnd(end) => {
                assert_eq!(end.op_id, op_id);
                receiver.finish().unwrap();
                assert_eq!(end.pushed_objects, arrived.len() as u64);
                fetched.incremental = end.incremental;
                break;
            }
            other => panic!("unexpected message during the push: {other:?}"),
        }
    }
    fetched.pushed = arrived.len() as u64;
    let mut kept = Vec::with_capacity(arrived.len());
    for (n, (id, data)) in arrived.into_iter().enumerate() {
        assert_eq!(ObjectId::of_canonical_bytes(&data), id, "object identity");
        if keep(n as u64) {
            kept.push((id, data));
        } else {
            fetched.dropped += 1;
        }
    }
    objects.put_batch(&kept, 8).await.unwrap();

    let mut delta = StructuralDelta::new(objects, since, want, StructuralDeltaLimits::default());
    while let Some(step) = delta.next().await.unwrap() {
        let DeltaStep::Missing(ids) = step else {
            continue;
        };
        let (pulled, chunked) = pull(ws, op_id, objects, fetched.wants, &ids).await;
        fetched.pulled += pulled;
        fetched.chunked += chunked;
        fetched.wants += 1;
        delta.resume();
    }
    send_msg(ws, &fetch_complete(op_id)).await;
    fetched
}

/// One want and its reply. Returns how many objects arrived, and how many of
/// those came as chunks.
async fn pull<O: ImmutableObjectStore>(
    ws: &mut Ws,
    op_id: u32,
    objects: &O,
    want_sequence: u32,
    ids: &[ObjectId],
) -> (u64, u64) {
    send_msg(ws, &object_want(op_id, want_sequence, ids)).await;
    let mut receiver = TransferReceiver::new(ids.iter().copied(), MAX_OBJECT_SIZE);
    let mut arrived = Vec::new();
    let mut chunked = 0;
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ObjectChunk(chunk) => {
                assert_eq!(chunk.op_id, op_id);
                let id = object_id(&chunk.object_id);
                if let Some(object) = receiver
                    .accept_chunk(id, chunk.offset, chunk.data, chunk.is_final)
                    .unwrap()
                {
                    chunked += 1;
                    arrived.push(object);
                }
            }
            WheatProtocolMessage::ObjectWantReply(reply) => {
                assert_eq!(reply.op_id, op_id);
                assert_eq!(reply.want_sequence, want_sequence);
                for entry in reply.objects {
                    let id = object_id(&entry.id);
                    receiver.accept_whole(id, entry.data.len()).unwrap();
                    arrived.push((id, entry.data));
                }
                receiver.finish_partial().unwrap();
                break;
            }
            other => panic!("unexpected message during a want: {other:?}"),
        }
    }
    assert!(!arrived.is_empty(), "a want reply must carry an object");
    for (id, data) in &arrived {
        assert_eq!(ObjectId::of_canonical_bytes(data), *id, "object identity");
    }
    objects.put_batch(&arrived, 8).await.unwrap();
    (arrived.len() as u64, chunked)
}

/// How a test client sends the objects a publish asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Upload {
    /// In `ObjectBatch`es within the batch limits.
    Whole,
    /// Each object in two `ObjectChunk`s, whatever its size.
    Chunked,
}

/// Answers a publish's `ObjectsNeeded` rounds until `ObjectsComplete`. Returns the
/// size of each round, and checks that no round is too large and that no object
/// is asked for twice.
pub async fn upload_objects<O: ImmutableObjectStore>(
    ws: &mut Ws,
    op_id: u32,
    objects: &O,
    upload: Upload,
) -> Vec<usize> {
    let mut asked = std::collections::HashSet::new();
    let mut rounds = Vec::new();
    loop {
        let ids = match recv_msg(ws).await {
            WheatProtocolMessage::ObjectsNeeded(needed) => {
                assert_eq!(needed.op_id, op_id);
                needed.ids
            }
            WheatProtocolMessage::ObjectsComplete(complete) => {
                assert_eq!(complete.op_id, op_id);
                return rounds;
            }
            other => panic!("expected ObjectsNeeded or ObjectsComplete, got {other:?}"),
        };
        assert!(
            !ids.is_empty() && ids.len() <= MAX_WANT_IDS,
            "{}",
            ids.len()
        );
        rounds.push(ids.len());
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        for digest in ids {
            let id = object_id(&digest);
            assert!(asked.insert(id), "{id} was asked for twice");
            let data = objects.get(id).await.unwrap().canonical_bytes.to_vec();
            if upload == Upload::Chunked {
                let half = data.len() / 2;
                send_msg(ws, &object_chunk(op_id, id, 0, &data[..half], false)).await;
                send_msg(ws, &object_chunk(op_id, id, half, &data[half..], true)).await;
                continue;
            }
            if batch.len() == MAX_OBJECT_BATCH_ITEMS
                || batch_bytes + data.len() > MAX_OBJECT_BATCH_SIZE
            {
                send_msg(ws, &object_batch(op_id, std::mem::take(&mut batch))).await;
                batch_bytes = 0;
            }
            batch_bytes += data.len();
            batch.push(ObjectEntry { id: digest, data });
        }
        if !batch.is_empty() {
            send_msg(ws, &object_batch(op_id, batch)).await;
        }
    }
}

pub fn object_batch(op_id: u32, objects: Vec<ObjectEntry>) -> WheatProtocolMessage {
    WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
        message_type: 0x21,
        op_id,
        sequence: 0,
        objects,
    })
}

pub fn object_chunk(
    op_id: u32,
    id: ObjectId,
    offset: usize,
    data: &[u8],
    is_final: bool,
) -> WheatProtocolMessage {
    WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
        message_type: 0x26,
        op_id,
        object_id: digest(id.as_bytes()),
        offset: offset as u64,
        data: data.to_vec(),
        is_final,
    })
}

/// Ends a publish's up-front push of `objects` objects and `contents` blobs.
/// One `ContentBatch` of `blobs`, each under its claimed ID.
pub fn content_batch(
    op_id: u32,
    blobs: impl IntoIterator<Item = (ContentId, Vec<u8>)>,
) -> WheatProtocolMessage {
    WheatProtocolMessage::ContentBatch(ContentBatchOutput {
        message_type: 0x31,
        op_id,
        sequence: 0,
        contents: blobs
            .into_iter()
            .map(|(id, data)| ContentEntry {
                id: digest(id.as_bytes()),
                data,
            })
            .collect(),
    })
}

pub fn push_end(op_id: u32, objects: u32, contents: u32) -> WheatProtocolMessage {
    WheatProtocolMessage::PublishPushEnd(PublishPushEndOutput {
        message_type: 0x4b,
        op_id,
        pushed_objects: objects,
        pushed_contents: contents,
    })
}

/// Sends a `PublishRequest` for `envelope` on top of the server's `expected` head,
/// with an empty push, so the server asks for everything.
pub async fn request_publish(
    ws: &mut Ws,
    op_id: u32,
    envelope: RepositoryEnvelopeId,
    expected: (u64, RepositoryEnvelopeId),
    force: bool,
) {
    request_publish_pushing(ws, op_id, envelope, expected, force).await;
    send_msg(ws, &push_end(op_id, 0, 0)).await;
}

/// Sends a `PublishRequest` and nothing after it: the caller pushes, then sends
/// [`push_end`].
pub async fn request_publish_pushing(
    ws: &mut Ws,
    op_id: u32,
    envelope: RepositoryEnvelopeId,
    expected: (u64, RepositoryEnvelopeId),
    force: bool,
) {
    send_msg(
        ws,
        &WheatProtocolMessage::PublishRequest(PublishRequestOutput {
            message_type: 0x40,
            op_id,
            publish_key: Uuid128 { bytes: vec![1; 16] },
            envelope_id: digest(envelope.as_bytes()),
            expected_generation: expected.0,
            expected_envelope: digest(expected.1.as_bytes()),
            force,
        }),
    )
    .await;
}

/// Fetches the content of `envelope` that `content` lacks. Returns how much.
pub async fn fetch_content<O: ImmutableObjectStore>(
    ws: &mut Ws,
    op_id: u32,
    objects: &O,
    content: &ContentStore,
    envelope: RepositoryEnvelopeId,
) -> usize {
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(envelope).await.unwrap();
    let state = reader.state(envelope.state).await.unwrap();

    let content_set = wheat_store::ContentSetStore::new(objects);
    let mut cursor = content_set.cursor(state.required_content).await.unwrap();
    let mut missing = Vec::new();
    while let Some(content_id) = cursor.next().await.unwrap() {
        if !content.contains(content_id) {
            missing.push(digest(content_id.as_bytes()));
        }
    }
    if missing.is_empty() {
        return 0;
    }
    let total = missing.len();
    let content_id = |d: &Digest256| ContentId::from_digest(d.bytes.as_slice().try_into().unwrap());
    let mut receiver = TransferReceiver::new(
        missing.iter().map(content_id).collect::<Vec<_>>(),
        MAX_CONTENT_SIZE,
    );
    send_msg(
        ws,
        &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
            message_type: 0x30,
            op_id,
            content_ids: missing,
        }),
    )
    .await;

    let store = |(id, data): (ContentId, Vec<u8>)| {
        assert_eq!(ContentId::of(&data), id, "content identity");
        content.put(&data).unwrap();
    };
    while !receiver.is_complete() {
        match recv_msg(ws).await {
            WheatProtocolMessage::ContentBatch(cb) => {
                assert_eq!(cb.op_id, op_id);
                for entry in cb.contents {
                    let id = content_id(&entry.id);
                    receiver.accept_whole(id, entry.data.len()).unwrap();
                    store((id, entry.data));
                }
            }
            WheatProtocolMessage::ContentChunk(chunk) => {
                assert_eq!(chunk.op_id, op_id);
                let id = content_id(&chunk.content_id);
                if let Some(whole) = receiver
                    .accept_chunk(id, chunk.offset, chunk.data, chunk.is_final)
                    .unwrap()
                {
                    store(whole);
                }
            }
            other => panic!("unexpected message during a content fetch: {other:?}"),
        }
    }
    receiver.finish().unwrap();
    total
}
