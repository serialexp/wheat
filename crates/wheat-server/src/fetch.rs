//! Fetch: push the structural objects the client lacks, then answer its wants.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use wheat_core::{ObjectId, RepositoryEnvelopeId};
use wheat_protocol::*;
use wheat_store::{
    DeltaStep, ImmutableObjectStore, ObjectKind, ObjectStoreError, StoredObject, StructuralDelta,
    StructuralDeltaLimits,
};

use crate::connection::ConnectionError;
use crate::registry::RepositoryEntry;
use crate::wire::{
    digest_to_object_id, operation_error_scope, recv, send, send_error, send_object_error,
};

/// Error code for a request the protocol does not allow.
const BAD_REQUEST: u16 = 400;
/// Error code for an object this repository does not have.
const NOT_FOUND: u16 = 404;
/// Error code for a repository whose own store is incomplete.
const INTERNAL: u16 = 500;

/// Handle a fetch.
///
/// Push: walk the structural objects of the wanted envelope that the client's
/// `since` envelope does not already cover ([`StructuralDelta`]), streaming them
/// as they are found, then send `FetchPushEnd`. Pull: answer each `ObjectWant`
/// with exactly one `ObjectWantReply`, until `FetchComplete`.
///
/// Any envelope in this repository's store may be fetched. A `since` envelope the
/// store does not have is ignored, and everything is pushed.
pub async fn handle_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    req: FetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    let want = digest_to_object_id(&req.want_envelope)?;
    if !is_envelope(&entry.store, want).await? {
        let message = "the wanted envelope is not in this repository";
        return send_object_error(ws, op_id, NOT_FOUND, message, want).await;
    }
    let since = digest_to_object_id(&req.since_envelope)?;
    let since = if *since.as_bytes() == [0; 32] || !is_envelope(&entry.store, since).await? {
        None
    } else {
        Some(RepositoryEnvelopeId::from_digest(*since.as_bytes()))
    };
    let want = RepositoryEnvelopeId::from_digest(*want.as_bytes());
    debug!(op_id, %want, incremental = since.is_some(), "fetch: starting");

    let Some(pushed) = push(ws, entry, op_id, since, want).await? else {
        return Ok(());
    };
    send(
        ws,
        &WheatProtocolMessage::FetchPushEnd(FetchPushEndOutput {
            message_type: 0x24,
            op_id,
            pushed_objects: pushed,
            incremental: since.is_some(),
        }),
    )
    .await?;
    debug!(op_id, pushed, "fetch: push done");

    let mut next_want = 0u32;
    loop {
        match recv(ws).await? {
            WheatProtocolMessage::ObjectWant(wanted) if wanted.op_id == op_id => {
                if wanted.want_sequence != next_want
                    || wanted.ids.is_empty()
                    || wanted.ids.len() > MAX_WANT_IDS
                {
                    let message = format!(
                        "an ObjectWant must be number {next_want} and ask for 1 to {MAX_WANT_IDS} objects"
                    );
                    return fetch_error(ws, op_id, BAD_REQUEST, &message).await;
                }
                if !answer_want(ws, entry, op_id, &wanted).await? {
                    return Ok(());
                }
                next_want += 1;
            }
            WheatProtocolMessage::FetchComplete(complete) if complete.op_id == op_id => {
                debug!(op_id, wants = next_want, "fetch: complete");
                return Ok(());
            }
            _ => {
                let message = "only ObjectWant and FetchComplete may follow FetchPushEnd";
                return fetch_error(ws, op_id, BAD_REQUEST, message).await;
            }
        }
    }
}

async fn fetch_error(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    code: u16,
    message: &str,
) -> Result<(), ConnectionError> {
    send_error(ws, op_id, code, operation_error_scope(), false, message).await
}

async fn is_envelope<S: ImmutableObjectStore + ?Sized>(
    objects: &S,
    id: ObjectId,
) -> Result<bool, ConnectionError> {
    match objects.get(id).await {
        Ok(object) => Ok(object.kind == ObjectKind::RepositoryEnvelope),
        Err(ObjectStoreError::Missing(_)) => Ok(false),
        Err(error) => Err(ConnectionError::Internal(format!(
            "failed to read {id}: {error}"
        ))),
    }
}

/// Streams the walk from `since` to `want`. Returns how many objects were pushed,
/// or `None` when the fetch ended with an error sent to the client.
#[tracing::instrument(name = "fetch_push", level = "debug", skip_all)]
async fn push(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    op_id: u32,
    since: Option<RepositoryEnvelopeId>,
    want: RepositoryEnvelopeId,
) -> Result<Option<u64>, ConnectionError> {
    let mut delta =
        StructuralDelta::new(&entry.store, since, want, StructuralDeltaLimits::default());
    let mut batch = BatchSender::new(op_id);
    let mut pushed = 0u64;
    while let Some(step) = delta
        .next()
        .await
        .map_err(|error| ConnectionError::Internal(format!("fetch walk failed: {error}")))?
    {
        match step {
            DeltaStep::Object(object) => {
                batch.push(ws, object).await?;
                pushed += 1;
            }
            DeltaStep::Missing(ids) => {
                // The repository's own head is incomplete; nothing the client can fix.
                tracing::warn!(op_id, missing = ids.len(), first = %ids[0], "fetch: store incomplete");
                let message = "this repository's store is missing an object of the envelope";
                send_object_error(ws, op_id, INTERNAL, message, ids[0]).await?;
                return Ok(None);
            }
        }
    }
    batch.flush(ws).await?;
    debug!(op_id, stats = ?delta.stats(), "fetch: walk done");
    Ok(Some(pushed))
}

/// Groups pushed objects into `ObjectBatch` messages, and sends objects too large
/// for one as `ObjectChunk`s.
struct BatchSender {
    op_id: u32,
    sequence: u32,
    objects: Vec<ObjectEntry>,
    bytes: usize,
}

impl BatchSender {
    fn new(op_id: u32) -> Self {
        Self {
            op_id,
            sequence: 0,
            objects: Vec::new(),
            bytes: 0,
        }
    }

    async fn push(
        &mut self,
        ws: &mut WebSocketStream<TcpStream>,
        object: StoredObject,
    ) -> Result<(), ConnectionError> {
        let len = object.canonical_bytes.len();
        if len > MAX_OBJECT_BATCH_SIZE {
            self.flush(ws).await?;
            return send_chunks(ws, self.op_id, &object).await;
        }
        // Flush first, so a batch never grows past its limits.
        if self.objects.len() == MAX_OBJECT_BATCH_ITEMS || self.bytes + len > MAX_OBJECT_BATCH_SIZE
        {
            self.flush(ws).await?;
        }
        self.bytes += len;
        self.objects.push(object_entry(&object));
        Ok(())
    }

    async fn flush(&mut self, ws: &mut WebSocketStream<TcpStream>) -> Result<(), ConnectionError> {
        if self.objects.is_empty() {
            return Ok(());
        }
        send(
            ws,
            &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                message_type: 0x21,
                op_id: self.op_id,
                sequence: self.sequence,
                objects: std::mem::take(&mut self.objects),
            }),
        )
        .await?;
        self.sequence += 1;
        self.bytes = 0;
        Ok(())
    }
}

fn object_entry(object: &StoredObject) -> ObjectEntry {
    ObjectEntry {
        id: Digest256 {
            bytes: object.id.as_bytes().to_vec(),
        },
        data: object.canonical_bytes.to_vec(),
    }
}

async fn send_chunks(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    object: &StoredObject,
) -> Result<(), ConnectionError> {
    let bytes = &object.canonical_bytes;
    let mut offset = 0;
    for piece in bytes.chunks(MAX_OBJECT_CHUNK_SIZE) {
        let end = offset + piece.len();
        send(
            ws,
            &WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
                message_type: 0x26,
                op_id,
                object_id: Digest256 {
                    bytes: object.id.as_bytes().to_vec(),
                },
                offset: offset as u64,
                data: piece.to_vec(),
                is_final: end == bytes.len(),
            }),
        )
        .await?;
        offset = end;
    }
    Ok(())
}

/// Answers one want with one reply: the wanted objects from the front of its list
/// that fit in a batch, the first one as chunks if it is too large for one.
/// Returns false when the fetch ended because an object is unknown.
#[tracing::instrument(name = "fetch_answer_want", level = "debug", skip_all)]
async fn answer_want(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    op_id: u32,
    wanted: &ObjectWantOutput,
) -> Result<bool, ConnectionError> {
    let mut objects = Vec::new();
    let mut bytes = 0;
    for (index, digest) in wanted.ids.iter().enumerate() {
        let id = digest_to_object_id(digest)?;
        let object = match entry.store.get(id).await {
            Ok(object) => object,
            Err(ObjectStoreError::Missing(_)) => {
                let message = "a wanted object is not in this repository";
                send_object_error(ws, op_id, NOT_FOUND, message, id).await?;
                return Ok(false);
            }
            Err(error) => {
                return Err(ConnectionError::Internal(format!(
                    "failed to read {id}: {error}"
                )));
            }
        };
        let len = object.canonical_bytes.len();
        if len > MAX_OBJECT_BATCH_SIZE {
            if index > 0 {
                break;
            }
            send_chunks(ws, op_id, &object).await?;
            continue;
        }
        if objects.len() == MAX_OBJECT_BATCH_ITEMS || bytes + len > MAX_OBJECT_BATCH_SIZE {
            break;
        }
        bytes += len;
        objects.push(object_entry(&object));
    }
    send(
        ws,
        &WheatProtocolMessage::ObjectWantReply(ObjectWantReplyOutput {
            message_type: 0x25,
            op_id,
            want_sequence: wanted.want_sequence,
            objects,
        }),
    )
    .await?;
    Ok(true)
}

/// Handle a content fetch request.
///
/// Reads all requested blobs together (one round trip on S3) and streams them
/// in batches. If that read fails, each blob is read on its own: one larger than
/// `MAX_CONTENT_CHUNK_SIZE` is sent as `ContentChunk`s, and one that cannot be
/// read ends the operation with an `Error`, because the client waits for every
/// blob it asked for.
pub async fn handle_content_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    req: ContentFetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    debug!(op_id, count = req.content_ids.len(), "content fetch");

    let mut ids = Vec::with_capacity(req.content_ids.len());
    for digest in &req.content_ids {
        ids.push(wheat_core::ContentId::from_digest(
            digest
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectionError::Protocol("invalid ContentId length"))?,
        ));
    }

    let mut batch = ContentBatcher::new(op_id);
    match entry.content.get_many_bounded(&ids, MAX_CONTENT_CHUNK_SIZE) {
        Ok(items) => {
            for (id, data) in items {
                if let Some(full) = batch.push(content_digest(id), data) {
                    send(ws, &full).await?;
                }
            }
        }
        Err(error) => {
            debug!(op_id, %error, "content fetch: bulk read failed, reading one at a time");
            for &id in &ids {
                let data = match entry.content.get_bounded(id, MAX_CONTENT_CHUNK_SIZE) {
                    Ok(data) => {
                        if let Some(full) = batch.push(content_digest(id), data) {
                            send(ws, &full).await?;
                        }
                        continue;
                    }
                    Err(wheat_store::ContentStoreError::TooLarge { .. }) => entry.content.get(id),
                    Err(error) => Err(error),
                };
                match data {
                    Ok(data) => {
                        if let Some(open) = batch.finish() {
                            send(ws, &open).await?;
                        }
                        let digest = content_digest(id);
                        for chunk in content_chunks(op_id, &digest, &data) {
                            send(ws, &chunk).await?;
                        }
                    }
                    Err(error) => {
                        debug!(op_id, content_id = %id, %error, "content fetch: unreadable");
                        let code = match error {
                            wheat_store::ContentStoreError::Missing(_) => NOT_FOUND,
                            _ => INTERNAL,
                        };
                        let message = format!("cannot read content {id}: {error}");
                        return send_content_error(ws, op_id, code, &message, id).await;
                    }
                }
            }
        }
    }
    match batch.finish() {
        Some(open) => send(ws, &open).await,
        None => Ok(()),
    }
}

fn content_digest(id: wheat_core::ContentId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

/// Send an operation-scoped, non-retryable Error about one blob.
async fn send_content_error(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    code: u16,
    message: &str,
    id: wheat_core::ContentId,
) -> Result<(), ConnectionError> {
    send(
        ws,
        &WheatProtocolMessage::Error(ErrorOutput {
            message_type: 0x70,
            op_id,
            code,
            scope: operation_error_scope(),
            retryable: false,
            message: message.to_string(),
            object_id: Digest256 {
                bytes: id.as_bytes().to_vec(),
            },
        }),
    )
    .await
}
