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
    if !is_envelope(&entry.objects, want).await? {
        let message = "the wanted envelope is not in this repository";
        return send_object_error(ws, op_id, NOT_FOUND, message, want).await;
    }
    let since = digest_to_object_id(&req.since_envelope)?;
    let since = if *since.as_bytes() == [0; 32] || !is_envelope(&entry.objects, since).await? {
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
    let mut delta = StructuralDelta::new(
        &entry.objects,
        since,
        want,
        StructuralDeltaLimits::default(),
    );
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
        let object = match entry.objects.get(id).await {
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
/// Fetches all requested content blobs concurrently (when the backend
/// supports it, e.g. S3) and then streams them as batched responses.
/// Large content that exceeds MAX_CONTENT_CHUNK_SIZE is still handled
/// individually via the chunked protocol.
pub async fn handle_content_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    req: ContentFetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    debug!(op_id, count = req.content_ids.len(), "content fetch");

    // Parse all content IDs up front.
    let mut normal_ids = Vec::new();
    let mut digest_map = Vec::new(); // parallel with normal_ids: original Digest256
    for digest in &req.content_ids {
        let content_id = wheat_core::ContentId::from_digest(
            digest
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectionError::Protocol("invalid ContentId length"))?,
        );
        normal_ids.push(content_id);
        digest_map.push(digest.clone());
    }

    // Fetch all content concurrently (one round-trip for S3 backends).
    let fetched = entry
        .content
        .get_many_bounded(&normal_ids, MAX_CONTENT_CHUNK_SIZE);

    let mut batch_contents = Vec::new();
    let mut batch_size = 0usize;
    let mut sequence = 0u32;

    match fetched {
        Ok(items) => {
            // All items fit within MAX_CONTENT_CHUNK_SIZE. Batch them for sending.
            for (content_id, data) in items {
                let digest = Digest256 {
                    bytes: content_id.as_bytes().to_vec(),
                };
                batch_size += data.len();
                batch_contents.push(ContentEntry { id: digest, data });

                if batch_size >= MAX_CONTENT_BATCH_SIZE
                    || batch_contents.len() >= MAX_CONTENT_BATCH_ITEMS
                {
                    send(
                        ws,
                        &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
                            message_type: 0x31,
                            op_id,
                            sequence,
                            contents: batch_contents,
                        }),
                    )
                    .await?;
                    batch_contents = Vec::new();
                    batch_size = 0;
                    sequence += 1;
                }
            }
        }
        Err(wheat_store::ContentStoreError::TooLarge { id, .. }) => {
            // At least one item is too large for bounded read. Fall back to
            // per-item processing so we can chunk oversized content.
            debug!(op_id, content_id = %id, "content fetch: oversized item, falling back to per-item");
            for (idx, content_id) in normal_ids.iter().enumerate() {
                match entry
                    .content
                    .get_bounded(*content_id, MAX_CONTENT_CHUNK_SIZE)
                {
                    Ok(data) => {
                        batch_size += data.len();
                        batch_contents.push(ContentEntry {
                            id: digest_map[idx].clone(),
                            data,
                        });

                        if batch_size >= MAX_CONTENT_BATCH_SIZE
                            || batch_contents.len() >= MAX_CONTENT_BATCH_ITEMS
                        {
                            send(
                                ws,
                                &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
                                    message_type: 0x31,
                                    op_id,
                                    sequence,
                                    contents: batch_contents,
                                }),
                            )
                            .await?;
                            batch_contents = Vec::new();
                            batch_size = 0;
                            sequence += 1;
                        }
                    }
                    Err(wheat_store::ContentStoreError::TooLarge { .. }) => {
                        // Flush any pending batch first.
                        if !batch_contents.is_empty() {
                            send(
                                ws,
                                &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
                                    message_type: 0x31,
                                    op_id,
                                    sequence,
                                    contents: batch_contents,
                                }),
                            )
                            .await?;
                            batch_contents = Vec::new();
                            batch_size = 0;
                            sequence += 1;
                        }

                        // Large content: read fully and send as chunks.
                        match entry.content.get(*content_id) {
                            Ok(data) => {
                                let data_len = data.len();
                                let mut offset = 0usize;
                                while offset < data_len {
                                    let end = (offset + MAX_CONTENT_CHUNK_SIZE).min(data_len);
                                    let chunk = data[offset..end].to_vec();
                                    let is_final = end == data_len;
                                    send(
                                        ws,
                                        &WheatProtocolMessage::ContentChunk(ContentChunkOutput {
                                            message_type: 0x32,
                                            op_id,
                                            content_id: digest_map[idx].clone(),
                                            offset: offset as u64,
                                            data: chunk,
                                            is_final,
                                        }),
                                    )
                                    .await?;
                                    offset = end;
                                }
                            }
                            Err(e) => {
                                debug!(op_id, content_id = %content_id, error = %e, "content fetch: large file read failed");
                            }
                        }
                    }
                    Err(e) => {
                        debug!(op_id, content_id = %content_id, error = %e, "content fetch: missing");
                    }
                }
            }
        }
        Err(e) => {
            // Other error (missing, IO, etc.) — fall back to per-item.
            debug!(op_id, error = %e, "content fetch: bulk read failed, falling back to per-item");
            for (idx, content_id) in normal_ids.iter().enumerate() {
                match entry
                    .content
                    .get_bounded(*content_id, MAX_CONTENT_CHUNK_SIZE)
                {
                    Ok(data) => {
                        batch_size += data.len();
                        batch_contents.push(ContentEntry {
                            id: digest_map[idx].clone(),
                            data,
                        });

                        if batch_size >= MAX_CONTENT_BATCH_SIZE
                            || batch_contents.len() >= MAX_CONTENT_BATCH_ITEMS
                        {
                            send(
                                ws,
                                &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
                                    message_type: 0x31,
                                    op_id,
                                    sequence,
                                    contents: batch_contents,
                                }),
                            )
                            .await?;
                            batch_contents = Vec::new();
                            batch_size = 0;
                            sequence += 1;
                        }
                    }
                    Err(e) => {
                        debug!(op_id, content_id = %content_id, error = %e, "content fetch: missing");
                    }
                }
            }
        }
    }

    // Flush remaining.
    if !batch_contents.is_empty() {
        send(
            ws,
            &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
                message_type: 0x31,
                op_id,
                sequence,
                contents: batch_contents,
            }),
        )
        .await?;
    }

    Ok(())
}
