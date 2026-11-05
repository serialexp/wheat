//! Fetch: push the structural objects the client lacks, then answer its wants.

use tracing::{Instrument as _, debug};

use wheat_core::{ObjectId, RepositoryEnvelopeId};
use wheat_protocol::*;
use wheat_store::{
    DeltaStep, ImmutableObjectStore, ObjectKind, ObjectStoreError, StoredObject, StructuralDelta,
    StructuralDeltaLimits,
};

use crate::blocking::drive;
use crate::connection::ConnectionError;
use crate::network::Network;
use crate::registry::RepositoryEntry;
use crate::transport::Transport;
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
    ws: &mut impl Transport,
    entry: &RepositoryEntry,
    req: FetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    let want = digest_to_object_id(&req.want_envelope)?;
    let since = digest_to_object_id(&req.since_envelope)?;
    let network = entry.network.clone();
    let (has_want, has_since) = drive(move || async move {
        let has_want = is_envelope(&network.store, want).await?;
        let has_since = *since.as_bytes() != [0; 32] && is_envelope(&network.store, since).await?;
        Ok::<_, ConnectionError>((has_want, has_since))
    })
    .await??;
    if !has_want {
        let message = "the wanted envelope is not in this repository";
        return send_object_error(ws, op_id, NOT_FOUND, message, want).await;
    }
    let since = has_since.then(|| RepositoryEnvelopeId::from_digest(*since.as_bytes()));
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

    let mut next_want = 0u64;
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
    ws: &mut impl Transport,
    op_id: u64,
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

/// How many messages the walk may have ready ahead of the connection sending them.
/// Each is at most one batch, so this bounds what a slow client makes the server hold.
const PUSH_QUEUE: usize = 2;

/// What the walk hands the connection to send.
enum Pushed {
    Message(WheatProtocolMessage),
    /// The walk reached an object the store does not have, and stopped.
    Missing(ObjectId),
}

/// Streams the walk from `since` to `want`. Returns how many objects were pushed,
/// or `None` when the fetch ended with an error sent to the client.
///
/// The walk reads the store, so it runs on a thread for blocking work and hands
/// the connection whole messages through a short queue.
#[tracing::instrument(name = "fetch_push", level = "debug", skip_all)]
async fn push(
    ws: &mut impl Transport,
    entry: &RepositoryEntry,
    op_id: u64,
    since: Option<RepositoryEnvelopeId>,
    want: RepositoryEnvelopeId,
) -> Result<Option<u64>, ConnectionError> {
    let (sender, mut queue) = tokio::sync::mpsc::channel(PUSH_QUEUE);
    let network = entry.network.clone();
    let span = tracing::debug_span!("fetch_walk");
    let walk = drive(move || {
        async move { walk(&network, op_id, since, want, &sender).await }.instrument(span)
    });
    // Polled with the queue, so the walk's result arrives without a task of its own.
    tokio::pin!(walk);
    let mut walked = None;
    loop {
        tokio::select! {
            item = queue.recv() => match item {
                Some(Pushed::Message(message)) => send(ws, &message).await?,
                Some(Pushed::Missing(id)) => {
                    // The repository's own head is incomplete; nothing the client can fix.
                    tracing::warn!(op_id, first = %id, "fetch: store incomplete");
                    let message = "this repository's store is missing an object of the envelope";
                    send_object_error(ws, op_id, INTERNAL, message, id).await?;
                    return Ok(None);
                }
                // The walk finished and sent everything; its result is next.
                None => break,
            },
            result = &mut walk, if walked.is_none() => walked = Some(result),
        }
    }
    let pushed = match walked {
        Some(result) => result,
        None => walk.await,
    }??;
    Ok(Some(pushed))
}

/// The walk from `since` to `want`, run on a thread for blocking work: groups what
/// it finds into messages for `sender`. Returns how many objects it pushed.
async fn walk(
    network: &Network,
    op_id: u64,
    since: Option<RepositoryEnvelopeId>,
    want: RepositoryEnvelopeId,
    sender: &tokio::sync::mpsc::Sender<Pushed>,
) -> Result<u64, ConnectionError> {
    let mut delta = StructuralDelta::new(
        &network.store,
        since,
        want,
        StructuralDeltaLimits::default(),
    );
    let mut batch = BatchSender::new(op_id);
    let mut ready = Vec::new();
    let mut pushed = 0u64;
    while let Some(step) = delta
        .next()
        .await
        .map_err(|error| ConnectionError::Internal(format!("fetch walk failed: {error}")))?
    {
        match step {
            DeltaStep::Object(object) => {
                batch.push(object, &mut ready);
                pushed += 1;
            }
            DeltaStep::Missing(ids) => {
                hand_over(sender, Pushed::Missing(ids[0])).await?;
                return Ok(pushed);
            }
        }
        for message in ready.drain(..) {
            hand_over(sender, Pushed::Message(message)).await?;
        }
    }
    batch.flush(&mut ready);
    for message in ready.drain(..) {
        hand_over(sender, Pushed::Message(message)).await?;
    }
    debug!(op_id, stats = ?delta.stats(), "fetch: walk done");
    Ok(pushed)
}

/// Hands `item` to the connection; fails once the connection has stopped taking them.
async fn hand_over(
    sender: &tokio::sync::mpsc::Sender<Pushed>,
    item: Pushed,
) -> Result<(), ConnectionError> {
    sender.send(item).await.map_err(|_| ConnectionError::Closed)
}

/// Groups pushed objects into `ObjectBatch` messages, and objects too large for
/// one into `ObjectChunk`s.
struct BatchSender {
    op_id: u64,
    sequence: u64,
    objects: Vec<ObjectEntry>,
    bytes: usize,
}

impl BatchSender {
    fn new(op_id: u64) -> Self {
        Self {
            op_id,
            sequence: 0,
            objects: Vec::new(),
            bytes: 0,
        }
    }

    /// Adds `object`, putting any messages that are now complete in `ready`.
    fn push(&mut self, object: StoredObject, ready: &mut Vec<WheatProtocolMessage>) {
        let len = object.canonical_bytes.len();
        if len > MAX_OBJECT_BATCH_SIZE {
            self.flush(ready);
            ready.extend(object_chunks(self.op_id, &object));
            return;
        }
        // Flush first, so a batch never grows past its limits.
        if self.objects.len() == MAX_OBJECT_BATCH_ITEMS || self.bytes + len > MAX_OBJECT_BATCH_SIZE
        {
            self.flush(ready);
        }
        self.bytes += len;
        self.objects.push(object_entry(&object));
    }

    /// Puts the batch being built, if any, in `ready`.
    fn flush(&mut self, ready: &mut Vec<WheatProtocolMessage>) {
        if self.objects.is_empty() {
            return;
        }
        ready.push(WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
            message_type: 0x21,
            op_id: self.op_id,
            sequence: self.sequence,
            objects: std::mem::take(&mut self.objects),
        }));
        self.sequence += 1;
        self.bytes = 0;
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

/// `object` as `ObjectChunk` messages, for one too large for a batch.
fn object_chunks(op_id: u64, object: &StoredObject) -> impl Iterator<Item = WheatProtocolMessage> {
    let bytes = &object.canonical_bytes;
    let total = bytes.len();
    bytes
        .chunks(MAX_OBJECT_CHUNK_SIZE)
        .scan(0, move |offset, piece| {
            let start = *offset;
            *offset += piece.len();
            Some(WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
                message_type: 0x26,
                op_id,
                object_id: Digest256 {
                    bytes: object.id.as_bytes().to_vec(),
                },
                offset: start as u64,
                data: piece.to_vec(),
                is_final: *offset == total,
            }))
        })
}

/// Answers one want with one reply: the wanted objects from the front of its list
/// that fit in a batch, the first one as chunks if it is too large for one.
/// Returns false when the fetch ended because an object is unknown.
#[tracing::instrument(name = "fetch_answer_want", level = "debug", skip_all)]
async fn answer_want(
    ws: &mut impl Transport,
    entry: &RepositoryEntry,
    op_id: u64,
    wanted: &ObjectWantOutput,
) -> Result<bool, ConnectionError> {
    let ids = wanted
        .ids
        .iter()
        .map(digest_to_object_id)
        .collect::<Result<Vec<_>, _>>()?;
    let network = entry.network.clone();
    let answer = drive(move || async move { read_want(&network, &ids).await }).await??;
    if let Some(object) = &answer.chunked {
        for chunk in object_chunks(op_id, object) {
            send(ws, &chunk).await?;
        }
    }
    if let Some(id) = answer.missing {
        let message = "a wanted object is not in this repository";
        send_object_error(ws, op_id, NOT_FOUND, message, id).await?;
        return Ok(false);
    }
    let objects = answer.objects;
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

/// What one want is answered with, read on a thread for blocking work.
struct WantAnswer {
    /// The first wanted object, when it is too large for a batch.
    chunked: Option<StoredObject>,
    /// The wanted objects that fit in the reply.
    objects: Vec<ObjectEntry>,
    /// The first wanted object the store does not have; the fetch ends at it.
    missing: Option<ObjectId>,
}

/// Reads the wanted objects from the front of `ids` that fit in one reply.
async fn read_want(network: &Network, ids: &[ObjectId]) -> Result<WantAnswer, ConnectionError> {
    let mut answer = WantAnswer {
        chunked: None,
        objects: Vec::new(),
        missing: None,
    };
    let mut bytes = 0;
    for (index, &id) in ids.iter().enumerate() {
        let object = match network.store.get(id).await {
            Ok(object) => object,
            Err(ObjectStoreError::Missing(_)) => {
                answer.missing = Some(id);
                return Ok(answer);
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
            answer.chunked = Some(object);
            continue;
        }
        if answer.objects.len() == MAX_OBJECT_BATCH_ITEMS || bytes + len > MAX_OBJECT_BATCH_SIZE {
            break;
        }
        bytes += len;
        answer.objects.push(object_entry(&object));
    }
    Ok(answer)
}

/// Handle a content fetch request.
///
/// Reads the requested blobs about a batch at a time (`read_group`) and sends
/// each group before reading the next, so an answer of thousands of chunks holds a
/// batch, not all of them. A blob up to `MAX_CONTENT_CHUNK_SIZE` goes in a batch;
/// a longer one is sent as `ContentChunk`s, one over the chunking threshold as its
/// `ContentManifest` while the answer's manifests fit `MAX_ANSWER_MANIFEST_BYTES`
/// (and is left out, for the client to ask again, once they do not), and one that
/// cannot be read ends the operation with an `Error`. `ContentFetchEnd` closes the
/// answer.
pub async fn handle_content_fetch(
    ws: &mut impl Transport,
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
    let mut manifests = ManifestBudget::default();
    let mut left_out = 0usize;
    let ids: std::sync::Arc<[wheat_core::ContentId]> = ids.into();
    let mut next = 0;
    while next < ids.len() {
        let network = entry.network.clone();
        let group = ids.clone();
        let (reads, read_to) =
            drive(move || async move { read_group(&network.content, &group, next) }).await?;
        next = read_to;
        for (id, read) in reads {
            match read {
                Ok(Fetched::Batched(data)) => {
                    if let Some(full) = batch.push(content_digest(id), data) {
                        send(ws, &full).await?;
                    }
                }
                Ok(Fetched::Whole(data)) => {
                    if let Some(open) = batch.finish() {
                        send(ws, &open).await?;
                    }
                    let digest = content_digest(id);
                    for chunk in content_chunks(op_id, &digest, &data) {
                        send(ws, &chunk).await?;
                    }
                }
                Ok(Fetched::Manifest(manifest)) => {
                    if manifests.admit(manifest.len()) {
                        send(ws, &content_manifest(op_id, content_digest(id), manifest)).await?;
                    } else {
                        left_out += 1;
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
    if let Some(open) = batch.finish() {
        send(ws, &open).await?;
    }
    if left_out > 0 {
        debug!(
            op_id,
            left_out, "content fetch: manifests left for the next request"
        );
    }
    send(
        ws,
        &WheatProtocolMessage::ContentFetchEnd(ContentFetchEndOutput {
            message_type: 0x35,
            op_id,
        }),
    )
    .await
}

/// A requested blob as the answer sends it.
#[derive(Debug)]
enum Fetched {
    /// Up to `MAX_CONTENT_CHUNK_SIZE`: an item of a `ContentBatch`.
    Batched(Vec<u8>),
    /// Longer, up to the chunking threshold: sent as `ContentChunk`s.
    Whole(Vec<u8>),
    /// Over the chunking threshold: its encoded manifest.
    Manifest(Vec<u8>),
}

impl Fetched {
    fn len(&self) -> usize {
        match self {
            Self::Batched(bytes) | Self::Whole(bytes) | Self::Manifest(bytes) => bytes.len(),
        }
    }
}

/// One requested blob and how reading it went.
type BlobRead = (
    wheat_core::ContentId,
    Result<Fetched, wheat_store::ContentStoreError>,
);

/// Reads `ids` from `start` on until a batch's worth of bytes is read or a blob
/// cannot be, and returns the reads and where the next group starts. A group holds
/// less than `MAX_CONTENT_BATCH_SIZE` plus one blob of at most `MAX_CONTENT_SIZE`.
fn read_group(
    content: &wheat_store::ContentStore,
    ids: &[wheat_core::ContentId],
    start: usize,
) -> (Vec<BlobRead>, usize) {
    let mut reads = Vec::new();
    let mut bytes = 0;
    let mut next = start;
    while next < ids.len() && bytes < MAX_CONTENT_BATCH_SIZE {
        let id = ids[next];
        next += 1;
        let read = match content.get_bounded(id, MAX_CONTENT_CHUNK_SIZE) {
            Ok(data) => Ok(Fetched::Batched(data)),
            Err(wheat_store::ContentStoreError::TooLarge { .. }) => long_blob(content, id),
            Err(error) => Err(error),
        };
        let failed = read.is_err();
        bytes += read.as_ref().map_or(0, Fetched::len);
        reads.push((id, read));
        if failed {
            break;
        }
    }
    (reads, next)
}

fn long_blob(
    content: &wheat_store::ContentStore,
    id: wheat_core::ContentId,
) -> Result<Fetched, wheat_store::ContentStoreError> {
    match content.chunk_manifest(id)? {
        Some(manifest) => wheat_core::encode_content_chunk_manifest(&manifest)
            .map(Fetched::Manifest)
            .map_err(|error| wheat_store::ContentStoreError::InvalidFormat {
                id,
                reason: error.to_string(),
            }),
        None => content
            .get_bounded(id, MAX_CONTENT_SIZE)
            .map(Fetched::Whole),
    }
}

fn content_digest(id: wheat_core::ContentId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

/// Send an operation-scoped, non-retryable Error about one blob.
async fn send_content_error(
    ws: &mut impl Transport,
    op_id: u64,
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
            retry_after_seconds: 0,
            message: message.to_string(),
            object_id: Digest256 {
                bytes: id.as_bytes().to_vec(),
            },
        }),
    )
    .await
}
#[cfg(test)]
mod tests {
    use wheat_core::ContentId;
    use wheat_store::ContentStore;

    use super::*;

    /// Many chunk-sized blobs are read a batch's worth at a time, never all at once.
    #[test]
    fn a_large_answer_is_read_a_batch_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let content = ContentStore::new(directory.path().join("content"));
        let mut ids = (0..16_u8)
            .map(|index| content.put(&vec![index; MAX_CONTENT_CHUNK_SIZE]).unwrap())
            .collect::<Vec<_>>();
        let missing = ContentId::from_digest([7; 32]);
        ids.insert(12, missing);

        let mut start = 0;
        let mut groups = Vec::new();
        loop {
            let (reads, next) = read_group(&content, &ids, start);
            let held: usize = reads
                .iter()
                .map(|(_, read)| read.as_ref().map_or(0, Fetched::len))
                .sum();
            assert!(
                held < MAX_CONTENT_BATCH_SIZE + MAX_CONTENT_SIZE,
                "a group held {held} bytes"
            );
            groups.push(reads.len());
            let failed = reads.last().is_some_and(|(_, read)| read.is_err());
            start = next;
            if failed {
                assert_eq!(reads.last().unwrap().0, missing);
                break;
            }
        }
        // Three 4 MiB blobs reach the 12 MiB batch size.
        assert_eq!(groups, [3, 3, 3, 3, 1]);
        assert_eq!(start, 13);
    }
}
