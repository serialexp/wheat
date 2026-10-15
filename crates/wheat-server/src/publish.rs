//! Publish protocol handler — receive objects, validate, commit.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

use std::collections::HashMap;

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};
use wheat_protocol::*;
use wheat_store::{
    BatchObjectStore, DeltaStep, ImmutableObjectStore, MerkleTreeStore, ObjectStoreError,
    PublicationError, PublicationStore, RepositoryHead, RepositoryReader, RepositoryUnionError,
    StructuralDelta, StructuralDeltaError, StructuralDeltaLimits, TransitionValidationOptions,
    TreeDiffLimits,
};

use crate::connection::ConnectionError;
use crate::registry::RepositoryEntry;
use crate::wire::{
    digest_to_content_id, digest_to_object_id, publish_error_status, publish_ok_status,
    publish_rejected_status, publish_result, publish_stale_status, recv, send,
};

// A blob the protocol accepts must also fit the store.
const _: () = assert!(MAX_CONTENT_SIZE == wheat_store::HARD_LOGICAL_RECORD_BYTES);

/// Handle a publish request. Returns the new head if publication succeeded.
pub async fn handle_publish(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    current_head: &RepositoryHead,
    req: PublishRequestOutput,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let op_id = req.op_id;
    let force = req.force;
    debug!(op_id, force, "publish: starting");

    // The head the client last saw. A forced publish replaces the head, so it must
    // be exactly this one. Any other publish is joined with whatever was published
    // since, at commit.
    let expected = RepositoryHead {
        generation: req.expected_generation,
        envelope: RepositoryEnvelopeId::from_digest(
            req.expected_envelope
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectionError::Protocol("invalid expected envelope ID"))?,
        ),
    };
    if force && expected != *current_head {
        return stale(
            ws,
            op_id,
            current_head,
            "a forced publish must name the current head",
        )
        .await;
    }

    let proposed_envelope_id = RepositoryEnvelopeId::from_digest(
        req.envelope_id
            .bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectionError::Protocol("invalid envelope ID"))?,
    );

    // Uploaded objects are staged and stored in one durable write once the publish
    // is accepted, rather than one per round of the walk. A refused publish leaves
    // nothing behind. Only a publish larger than the stage is written early, in
    // stage-sized batches, and those objects are harmless if it is then refused.
    let staged = BatchObjectStore::new(&*entry.objects, STAGED_OBJECTS, STAGED_OBJECT_BYTES);

    // --- Object transfer phase ---
    if let Some(refusal) =
        receive_objects(ws, &staged, op_id, current_head, proposed_envelope_id).await?
    {
        return reject(ws, op_id, current_head, &refusal).await;
    }

    // --- Content transfer phase ---
    let reader = RepositoryReader::new(&staged, &entry.content);
    let envelope_record = reader
        .envelope(proposed_envelope_id)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read proposed envelope: {e}")))?;
    let state = reader
        .state(envelope_record.state)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read proposed state: {e}")))?;
    let manifest = reader
        .manifest(state.required_content)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read content manifest: {e}")))?;
    let current_envelope = reader
        .envelope(current_head.envelope)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read current envelope: {e}")))?;
    let current_state = reader
        .state(current_envelope.state)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read current state: {e}")))?;
    let current_manifest = reader
        .manifest(current_state.required_content)
        .await
        .map_err(|e| {
            ConnectionError::Internal(format!("failed to read current content manifest: {e}"))
        })?;

    // A head is only accepted once all its content is stored, so only content the
    // current head lacks can be missing (transition validation relies on the same).
    // Bulk-check those: for S3 backends this does one LIST call instead of one GET
    // per item.
    let candidates = wheat_store::ContentSetStore::new(&staged)
        .added(current_manifest.content, manifest.content)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to compare content sets: {e}")))?;
    let existing = entry
        .content
        .existing_ids(&candidates)
        .map_err(|e| ConnectionError::Internal(format!("content existence check failed: {e}")))?;
    let missing: Vec<ContentId> = candidates
        .iter()
        .filter(|id| !existing.contains(id))
        .copied()
        .collect();

    if !missing.is_empty() {
        // Diff the old and new trees to find which files changed, then use
        // the old ContentId as a delta base for each new ContentId. This
        // produces much smaller stored objects for typical edits.
        let base_map = compute_delta_base_map(&staged, &reader, &current_state, &state).await;

        // Ask in rounds of at most MAX_WANT_IDS, one ContentNeeded each. In
        // every round accept only the requested blobs, each once, and store
        // them in batches as they arrive so the session never holds more
        // than one batch plus the blob being reassembled.
        let mut pending = PendingContent::default();
        for round in missing.chunks(MAX_WANT_IDS) {
            send(
                ws,
                &WheatProtocolMessage::ContentNeeded(ContentNeededOutput {
                    message_type: 0x43,
                    op_id,
                    content_ids: round
                        .iter()
                        .map(|id| Digest256 {
                            bytes: id.as_bytes().to_vec(),
                        })
                        .collect(),
                }),
            )
            .await?;
            let mut receiver = TransferReceiver::new(round.iter().copied(), MAX_CONTENT_SIZE);
            loop {
                let msg = recv(ws).await?;
                let received = match msg {
                    WheatProtocolMessage::ContentUpload(upload) if upload.op_id == op_id => {
                        let id = digest_to_content_id(&upload.content_id)?;
                        if let Err(e) = receiver.accept_whole(id, upload.data.len()) {
                            return reject(ws, op_id, current_head, &format!("{e}")).await;
                        }
                        Some((id, upload.data))
                    }
                    WheatProtocolMessage::ContentUploadChunk(chunk) if chunk.op_id == op_id => {
                        let id = digest_to_content_id(&chunk.content_id)?;
                        match receiver.accept_chunk(id, chunk.offset, chunk.data, chunk.is_final) {
                            Ok(finished) => finished,
                            Err(e) => {
                                return reject(ws, op_id, current_head, &format!("{e}")).await;
                            }
                        }
                    }
                    WheatProtocolMessage::ContentUploadComplete(cuc) if cuc.op_id == op_id => {
                        debug!(op_id, "publish: content upload round complete");
                        if let Err(e) = receiver.finish() {
                            return reject(ws, op_id, current_head, &format!("{e}")).await;
                        }
                        break;
                    }
                    _ => {
                        warn!(op_id, "publish: unexpected message during content upload");
                        None
                    }
                };
                if let Some((id, data)) = received {
                    if ContentId::of(&data) != id {
                        return reject(ws, op_id, current_head, "content identity mismatch").await;
                    }
                    let base = base_map.get(&id).copied();
                    if pending.push(id, data, base) {
                        pending.store(entry)?;
                    }
                }
            }
        }
        pending.store(entry)?;
    }

    // Nothing more is needed; the client may commit.
    send(
        ws,
        &WheatProtocolMessage::ContentComplete(ContentCompleteOutput {
            message_type: 0x4a,
            op_id,
        }),
    )
    .await?;

    // --- Commit phase ---
    loop {
        let msg = recv(ws).await?;
        match msg {
            WheatProtocolMessage::PublishCommit(pc) => {
                if pc.op_id == op_id {
                    debug!(op_id, "publish: commit requested");
                    break;
                }
            }
            _ => {
                warn!(op_id, "publish: unexpected message waiting for commit");
            }
        }
    }

    send(
        ws,
        &WheatProtocolMessage::PublishProgress(PublishProgressOutput {
            message_type: 0x48,
            op_id,
            phase: "validating".to_string(),
            percent: 255, // indeterminate
        }),
    )
    .await?;
    commit(
        ws,
        entry,
        &staged,
        &reader,
        op_id,
        expected,
        proposed_envelope_id,
        force,
    )
    .await
}

/// Times one publish retries its compare-and-swap when another process sharing the
/// store moved the head in between. Publishes through this server take turns on
/// the entry's commit lock, so they never cause a retry.
const MAX_COMMIT_ATTEMPTS: usize = 16;

/// Makes `proposed` the head, joined with whatever was published since `expected`,
/// and tells the client the new head.
///
/// The join is the one a fetch does: the union of both states, with labels merged
/// three ways against `expected`. So writers of unrelated changes never have to
/// fetch and try again; only a label both sides moved, or a change recorded
/// differently on each side, sends the publish back as stale. A forced publish is
/// never joined: it replaces exactly the head it names.
#[tracing::instrument(name = "publish_commit", level = "debug", skip_all)]
async fn commit(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    staged: &Staged<'_>,
    reader: &RepositoryReader<'_, Staged<'_>>,
    op_id: u32,
    expected: RepositoryHead,
    proposed: RepositoryEnvelopeId,
    force: bool,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let _turn = entry.commit.lock().await;
    let internal = |what: &str, e: &dyn std::fmt::Display| {
        ConnectionError::Internal(format!("publish: {what}: {e}"))
    };
    for _ in 0..MAX_COMMIT_ATTEMPTS {
        let head = entry
            .heads
            .read()
            .map_err(|e| internal("read the head", &e))?;
        let joined = if head.envelope == expected.envelope {
            proposed
        } else if force {
            return stale(
                ws,
                op_id,
                &head,
                "a forced publish must name the current head",
            )
            .await;
        } else {
            match reader
                .union_envelopes(Some(expected.envelope), proposed, head.envelope)
                .await
            {
                Ok(joined) => joined,
                Err(
                    e @ (RepositoryUnionError::DifferentBaseView { .. }
                    | RepositoryUnionError::DivergentChanges(_)
                    | RepositoryUnionError::DivergentLabels(_)),
                ) => {
                    let message = format!("{e}; fetch and resolve, then publish again");
                    return stale(ws, op_id, &head, &message).await;
                }
                Err(e) => return Err(internal("join with the current head", &e)),
            }
        };

        let from = reader
            .envelope(head.envelope)
            .await
            .map_err(|e| internal("read the current envelope", &e))?;
        let to = reader
            .envelope(joined)
            .await
            .map_err(|e| internal("read the joined envelope", &e))?;
        let options = TransitionValidationOptions {
            allow_non_monotonic: force,
        };
        if let Err(e) = reader
            .validate_transition_with_options(from.state, to.state, options)
            .await
        {
            info!(op_id, error = %e, "publish: validation failed");
            return reject(ws, op_id, &head, &format!("validation failed: {e}")).await;
        }

        // Store the staged objects, then seal any buffered objects into a durable
        // batch (e.g. a packed segment uploaded to S3) before the head can name
        // them. Sealing is a no-op for local stores.
        let stored = match staged.flush().await {
            Ok(()) => entry.objects.seal().await,
            Err(e) => Err(e),
        };
        if let Err(e) = stored {
            warn!(op_id, error = %e, "publish: seal failed");
            let message = format!("storing the published objects failed: {e}");
            send(
                ws,
                &publish_result(
                    op_id,
                    publish_error_status(),
                    head.generation,
                    head.envelope.as_bytes(),
                    &message,
                ),
            )
            .await?;
            return Ok(None);
        }

        let new_head = RepositoryHead {
            generation: head.generation + 1,
            envelope: joined,
        };
        match entry.heads.compare_and_advance(head, new_head) {
            Ok(()) => {
                info!(
                    op_id,
                    generation = new_head.generation,
                    joined = joined != proposed,
                    "publish: committed"
                );
                send(
                    ws,
                    &publish_result(
                        op_id,
                        publish_ok_status(),
                        new_head.generation,
                        joined.as_bytes(),
                        "",
                    ),
                )
                .await?;
                return Ok(Some(new_head));
            }
            // Another process sharing the store moved the head: join again.
            Err(PublicationError::Stale { .. }) => continue,
            Err(e) => {
                send(
                    ws,
                    &publish_result(
                        op_id,
                        publish_error_status(),
                        head.generation,
                        head.envelope.as_bytes(),
                        &format!("head advance failed: {e}"),
                    ),
                )
                .await?;
                return Ok(None);
            }
        }
    }
    let head = entry
        .heads
        .read()
        .map_err(|e| internal("read the head", &e))?;
    stale(
        ws,
        op_id,
        &head,
        "the head kept moving under this publish; try again",
    )
    .await
}

/// Send the publish back as stale: the client must fetch first.
async fn stale(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    head: &RepositoryHead,
    message: &str,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    info!(op_id, message, "publish: stale");
    send(
        ws,
        &publish_result(
            op_id,
            publish_stale_status(),
            head.generation,
            head.envelope.as_bytes(),
            message,
        ),
    )
    .await?;
    Ok(None)
}

/// Refuse the publish: tell the client, leave the head where it is.
async fn reject(
    ws: &mut WebSocketStream<TcpStream>,
    op_id: u32,
    current_head: &RepositoryHead,
    message: &str,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    info!(op_id, message, "publish: refused");
    send(
        ws,
        &publish_result(
            op_id,
            publish_rejected_status(),
            current_head.generation,
            current_head.envelope.as_bytes(),
            message,
        ),
    )
    .await?;
    Ok(None)
}

/// Uploaded bytes held before they are stored together. Storing in batches
/// keeps concurrent writes for remote backends; the cap keeps a publish
/// from holding every uploaded blob in memory at once.
const PENDING_CONTENT_BYTES: usize = 64 * 1024 * 1024;

/// Uploaded content waiting to be stored, with its delta base.
#[derive(Default)]
struct PendingContent {
    items: Vec<(ContentId, Vec<u8>, Option<ContentId>)>,
    bytes: usize,
}

impl PendingContent {
    /// Add a blob. Returns true once the batch is full and should be stored.
    fn push(&mut self, id: ContentId, data: Vec<u8>, base: Option<ContentId>) -> bool {
        self.bytes += data.len();
        self.items.push((id, data, base));
        self.bytes >= PENDING_CONTENT_BYTES
    }

    /// Store everything pending, with delta compression where a base is known.
    fn store(&mut self, entry: &RepositoryEntry) -> Result<(), ConnectionError> {
        if self.items.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.items);
        self.bytes = 0;
        entry
            .content
            .put_many_with_bases(items, 8)
            .map_err(|e| ConnectionError::Internal(format!("failed to store content batch: {e}")))
    }
}

/// Writers used to store one message's worth of uploaded objects.
const OBJECT_WRITE_CONCURRENCY: usize = 8;

/// A publish's uploaded objects, held until the publish is accepted. Reads see them
/// before they are stored.
type Staged<'a> = BatchObjectStore<'a, dyn ImmutableObjectStore>;

/// Most objects one publish stages before writing them early.
const STAGED_OBJECTS: usize = 65_536;
/// Most object bytes one publish stages before writing them early, like
/// [`PENDING_CONTENT_BYTES`] for content.
const STAGED_OBJECT_BYTES: usize = 64 * 1024 * 1024;

/// Asks for the objects of `proposed` that the store lacks, until it has them all.
///
/// Walks from the current head to `proposed` ([`StructuralDelta`]). Each batch of
/// objects the walk finds missing becomes one `ObjectsNeeded`, and the walk goes on
/// only once every object of that batch has arrived. `ObjectsComplete` follows when
/// the walk ends: the store then holds all of `proposed` that the current head does
/// not already cover. Returns why the publish is refused, if it is.
#[tracing::instrument(name = "publish_objects", level = "debug", skip_all)]
async fn receive_objects(
    ws: &mut WebSocketStream<TcpStream>,
    staged: &Staged<'_>,
    op_id: u32,
    current_head: &RepositoryHead,
    proposed: RepositoryEnvelopeId,
) -> Result<Option<String>, ConnectionError> {
    let mut delta = StructuralDelta::new(
        staged,
        Some(current_head.envelope),
        proposed,
        StructuralDeltaLimits::default(),
    );
    let mut rounds = 0u32;
    loop {
        let step = match delta.next().await {
            Ok(step) => step,
            // Reading the store, or the current head's side of the walk, failed.
            Err(
                error @ (StructuralDeltaError::Store(_)
                | StructuralDeltaError::Collection(_)
                | StructuralDeltaError::OldLineage(_)),
            ) => {
                return Err(ConnectionError::Internal(format!(
                    "publish walk failed: {error}"
                )));
            }
            Err(error) => return Ok(Some(format!("the proposed envelope is invalid: {error}"))),
        };
        let ids = match step {
            None => break,
            // Already in the store.
            Some(DeltaStep::Object(_)) => continue,
            Some(DeltaStep::Missing(ids)) => ids,
        };
        send(
            ws,
            &WheatProtocolMessage::ObjectsNeeded(ObjectsNeededOutput {
                message_type: 0x41,
                op_id,
                ids: ids
                    .iter()
                    .map(|id| Digest256 {
                        bytes: id.as_bytes().to_vec(),
                    })
                    .collect(),
            }),
        )
        .await?;
        if let Some(refusal) = receive_round(ws, staged, op_id, &ids).await? {
            return Ok(Some(refusal));
        }
        rounds += 1;
        delta.resume();
    }
    debug!(op_id, rounds, stats = ?delta.stats(), "publish: objects complete");
    send(
        ws,
        &WheatProtocolMessage::ObjectsComplete(ObjectsCompleteOutput {
            message_type: 0x42,
            op_id,
        }),
    )
    .await?;
    Ok(None)
}

/// Takes one round of uploads: each object of `ids` once, whole in an
/// `ObjectBatch` or in `ObjectChunk`s, and nothing else. Returns why the round is
/// refused, if it is.
async fn receive_round(
    ws: &mut WebSocketStream<TcpStream>,
    staged: &Staged<'_>,
    op_id: u32,
    ids: &[ObjectId],
) -> Result<Option<String>, ConnectionError> {
    let mut receiver = TransferReceiver::new(ids.iter().copied(), MAX_OBJECT_SIZE);
    while !receiver.is_complete() {
        match recv(ws).await? {
            WheatProtocolMessage::ObjectBatch(batch) if batch.op_id == op_id => {
                let mut objects = Vec::with_capacity(batch.objects.len());
                for object in batch.objects {
                    let id = digest_to_object_id(&object.id)?;
                    if let Err(error) = receiver.accept_whole(id, object.data.len()) {
                        return Ok(Some(error.to_string()));
                    }
                    if ObjectId::of_canonical_bytes(&object.data) != id {
                        return Ok(Some(format!("object identity mismatch for {id}")));
                    }
                    objects.push((id, object.data));
                }
                if let Some(refusal) = store_objects(staged, &objects).await? {
                    return Ok(Some(refusal));
                }
            }
            WheatProtocolMessage::ObjectChunk(chunk) if chunk.op_id == op_id => {
                let id = digest_to_object_id(&chunk.object_id)?;
                let object =
                    match receiver.accept_chunk(id, chunk.offset, chunk.data, chunk.is_final) {
                        Ok(Some(object)) => object,
                        Ok(None) => continue,
                        Err(error) => return Ok(Some(error.to_string())),
                    };
                if ObjectId::of_canonical_bytes(&object.1) != id {
                    return Ok(Some(format!("object identity mismatch for {id}")));
                }
                if let Some(refusal) = store_objects(staged, &[object]).await? {
                    return Ok(Some(refusal));
                }
            }
            _ => {
                return Ok(Some(
                    "only ObjectBatch and ObjectChunk may answer ObjectsNeeded".to_owned(),
                ));
            }
        }
    }
    Ok(None)
}

/// Stages uploaded objects. Returns why they are refused when they are not valid
/// Wheat objects.
async fn store_objects(
    staged: &Staged<'_>,
    objects: &[(ObjectId, Vec<u8>)],
) -> Result<Option<String>, ConnectionError> {
    match staged.put_batch(objects, OBJECT_WRITE_CONCURRENCY).await {
        Ok(()) => Ok(None),
        Err(error @ ObjectStoreError::InvalidFormat(_)) => Ok(Some(error.to_string())),
        Err(error) => Err(ConnectionError::Internal(format!(
            "failed to store objects: {error}"
        ))),
    }
}

/// Diff old and new Merkle trees to find `new_content → old_content` base
/// mappings for delta compression. Falls back to an empty map on any error
/// (delta compression is an optimization, not a correctness requirement).
async fn compute_delta_base_map(
    objects: &Staged<'_>,
    reader: &RepositoryReader<'_, Staged<'_>>,
    old_state: &wheat_core::RepositoryStateRecord,
    new_state: &wheat_core::RepositoryStateRecord,
) -> HashMap<ContentId, ContentId> {
    let mut base_map = HashMap::new();

    let old_view = match reader.view(old_state.base_view).await {
        Ok(v) => v,
        Err(_) => return base_map,
    };
    let new_view = match reader.view(new_state.base_view).await {
        Ok(v) => v,
        Err(_) => return base_map,
    };

    if old_view.tree == new_view.tree {
        return base_map; // No tree changes.
    }

    // Walk the diff — O(changed files), skips identical subtrees.
    let trees = MerkleTreeStore::new(objects);
    let mut diff_cursor =
        trees.diff_cursor(old_view.tree, new_view.tree, TreeDiffLimits::default());
    while let Ok(Some(diff)) = diff_cursor.next().await {
        if let (Some(old_value), Some(new_value)) = (&diff.old, &diff.new)
            && old_value.content != new_value.content
        {
            base_map.insert(new_value.content, old_value.content);
        }
    }

    base_map
}
