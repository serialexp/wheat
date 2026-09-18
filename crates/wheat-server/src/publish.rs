//! Publish protocol handler — receive objects, validate, commit.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};
use wheat_protocol::*;
use wheat_store::{
    ImmutableObjectStore, PublicationError, PublicationStore, RepositoryHead, RepositoryReader,
    TransitionValidationOptions,
};

use crate::connection::ConnectionError;
use crate::registry::RepositoryEntry;
use crate::wire::{
    digest_to_content_id, digest_to_object_id, publish_error_status, publish_ok_status,
    publish_rejected_status, publish_result, publish_stale_status, recv, send,
};

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

    // Verify expected generation matches current head.
    if req.expected_generation != current_head.generation {
        send(
            ws,
            &publish_result(
                op_id,
                publish_stale_status(),
                current_head.generation,
                current_head.envelope.as_bytes(),
                "expected generation does not match current head",
            ),
        )
        .await?;
        return Ok(None);
    }

    // Verify expected envelope matches current head.
    let expected_envelope_bytes: [u8; 32] = req
        .expected_envelope
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| ConnectionError::Protocol("invalid expected envelope ID"))?;
    if expected_envelope_bytes != *current_head.envelope.as_bytes() {
        send(
            ws,
            &publish_result(
                op_id,
                publish_stale_status(),
                current_head.generation,
                current_head.envelope.as_bytes(),
                "expected envelope does not match current head",
            ),
        )
        .await?;
        return Ok(None);
    }

    let proposed_envelope_id = RepositoryEnvelopeId::from_digest(
        req.envelope_id
            .bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectionError::Protocol("invalid envelope ID"))?,
    );

    // --- Object transfer phase ---
    // Discover what's missing and request it. If everything's already present
    // (e.g. retried publish with same envelope), skip straight to complete.
    let envelope_oid: ObjectId = proposed_envelope_id.into();

    let mut objects_complete = false;
    let missing = discover_missing_objects(entry, proposed_envelope_id, envelope_oid).await;
    if missing.is_empty() {
        // Server already has the full closure (retry of a prior attempt).
        send(
            ws,
            &WheatProtocolMessage::ObjectsComplete(ObjectsCompleteOutput {
                message_type: 0x42,
                op_id,
            }),
        )
        .await?;
        objects_complete = true;
    } else {
        let batch_size = missing.len().min(MAX_WANT_IDS);
        send(
            ws,
            &WheatProtocolMessage::ObjectsNeeded(ObjectsNeededOutput {
                message_type: 0x41,
                op_id,
                ids: missing[..batch_size].to_vec(),
            }),
        )
        .await?;
    }

    // Receive objects until we have the full closure.
    while !objects_complete {
        let msg = recv(ws).await?;
        match msg {
            WheatProtocolMessage::ObjectBatch(batch) => {
                if batch.op_id != op_id {
                    continue;
                }
                debug!(
                    op_id,
                    count = batch.objects.len(),
                    sequence = batch.sequence,
                    "publish: received ObjectBatch"
                );

                // Store each object, validating identity.
                for obj_entry in &batch.objects {
                    let claimed_id = digest_to_object_id(&obj_entry.id)?;
                    let actual_id = ObjectId::of_canonical_bytes(&obj_entry.data);
                    if actual_id != claimed_id {
                        send(
                            ws,
                            &publish_result(
                                op_id,
                                publish_rejected_status(),
                                current_head.generation,
                                current_head.envelope.as_bytes(),
                                "object identity mismatch",
                            ),
                        )
                        .await?;
                        return Ok(None);
                    }
                    entry
                        .objects
                        .put_if_absent(claimed_id, &obj_entry.data)
                        .await
                        .map_err(|e| {
                            ConnectionError::Internal(format!("failed to store object: {e}"))
                        })?;
                }

                // Re-check what's still missing.
                let missing =
                    discover_missing_objects(entry, proposed_envelope_id, envelope_oid).await;
                if missing.is_empty() {
                    send(
                        ws,
                        &WheatProtocolMessage::ObjectsComplete(ObjectsCompleteOutput {
                            message_type: 0x42,
                            op_id,
                        }),
                    )
                    .await?;
                    objects_complete = true;
                } else {
                    let batch_size = missing.len().min(MAX_WANT_IDS);
                    send(
                        ws,
                        &WheatProtocolMessage::ObjectsNeeded(ObjectsNeededOutput {
                            message_type: 0x41,
                            op_id,
                            ids: missing[..batch_size].to_vec(),
                        }),
                    )
                    .await?;
                }
            }
            _ => {
                warn!(op_id, "publish: unexpected message during object transfer");
            }
        }
    }

    // --- Content transfer phase ---
    let reader = RepositoryReader::new(&entry.objects, &entry.content);
    let envelope_record = reader.envelope(proposed_envelope_id).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read proposed envelope: {e}"))
    })?;
    let state = reader.state(envelope_record.state).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read proposed state: {e}"))
    })?;
    let manifest = reader
        .manifest(state.required_content)
        .await
        .map_err(|e| {
            ConnectionError::Internal(format!("failed to read content manifest: {e}"))
        })?;

    // Walk the content set to find missing content using contains() —
    // NOT get_bounded(id, 0), which would return TooLarge for every non-empty file.
    let content_set = wheat_store::ContentSetStore::new(&entry.objects);
    let mut cursor = content_set.cursor(manifest.content).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to walk content set: {e}"))
    })?;
    let mut missing_content = Vec::new();
    while let Some(content_id) = cursor.next().await.map_err(|e| {
        ConnectionError::Internal(format!("content set walk error: {e}"))
    })? {
        if !entry.content.contains(content_id) {
            missing_content.push(Digest256 {
                bytes: content_id.as_bytes().to_vec(),
            });
        }
    }

    if !missing_content.is_empty() {
        let batch_size = missing_content.len().min(MAX_WANT_IDS);
        send(
            ws,
            &WheatProtocolMessage::ContentNeeded(ContentNeededOutput {
                message_type: 0x43,
                op_id,
                content_ids: missing_content[..batch_size].to_vec(),
            }),
        )
        .await?;

        // Receive content uploads (single-message and chunked).
        loop {
            let msg = recv(ws).await?;
            match msg {
                WheatProtocolMessage::ContentUpload(upload) => {
                    if upload.op_id != op_id {
                        continue;
                    }
                    let content_id = digest_to_content_id(&upload.content_id)?;
                    let actual_id = ContentId::of(&upload.data);
                    if actual_id != content_id {
                        send(
                            ws,
                            &publish_result(
                                op_id,
                                publish_rejected_status(),
                                current_head.generation,
                                current_head.envelope.as_bytes(),
                                "content identity mismatch",
                            ),
                        )
                        .await?;
                        return Ok(None);
                    }
                    entry.content.put(&upload.data).map_err(|e| {
                        ConnectionError::Internal(format!("failed to store content: {e}"))
                    })?;
                }
                WheatProtocolMessage::ContentUploadChunk(chunk) => {
                    if chunk.op_id != op_id {
                        continue;
                    }
                    // For v1: accumulate chunks in memory, verify on final chunk.
                    // TODO: stream to disk for very large files.
                    // For now, we only handle single-chunk "large" uploads that
                    // fit in memory — the is_final flag must be true.
                    if chunk.is_final && chunk.offset == 0 {
                        // Single-chunk case: treat like ContentUpload.
                        let content_id = digest_to_content_id(&chunk.content_id)?;
                        let actual_id = ContentId::of(&chunk.data);
                        if actual_id != content_id {
                            send(
                                ws,
                                &publish_result(
                                    op_id,
                                    publish_rejected_status(),
                                    current_head.generation,
                                    current_head.envelope.as_bytes(),
                                    "chunked content identity mismatch",
                                ),
                            )
                            .await?;
                            return Ok(None);
                        }
                        entry.content.put(&chunk.data).map_err(|e| {
                            ConnectionError::Internal(format!(
                                "failed to store chunked content: {e}"
                            ))
                        })?;
                    } else {
                        // Multi-chunk reassembly: not yet implemented.
                        // For v1 this is acceptable — the client can always
                        // fall back to single-message ContentUpload for files
                        // under 4 MiB.
                        warn!(
                            op_id,
                            offset = chunk.offset,
                            "publish: multi-chunk content reassembly not yet implemented"
                        );
                    }
                }
                WheatProtocolMessage::ContentUploadComplete(cuc) => {
                    if cuc.op_id == op_id {
                        debug!(op_id, "publish: content upload complete");
                        break;
                    }
                }
                _ => {
                    warn!(op_id, "publish: unexpected message during content upload");
                }
            }
        }
    }

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

    // Run bounded delta validation.
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

    let expected_envelope = reader
        .envelope(current_head.envelope)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read current envelope: {e}")))?;

    let validation_options = TransitionValidationOptions {
        allow_non_monotonic: force,
    };

    if let Err(e) = reader
        .validate_transition_with_options(
            expected_envelope.state,
            envelope_record.state,
            validation_options,
        )
        .await
    {
        info!(op_id, error = %e, "publish: validation failed");
        send(
            ws,
            &publish_result(
                op_id,
                publish_rejected_status(),
                current_head.generation,
                current_head.envelope.as_bytes(),
                &format!("validation failed: {e}"),
            ),
        )
        .await?;
        return Ok(None);
    }

    // CAS the head.
    let new_generation = current_head.generation + 1;
    let proposed_head = RepositoryHead {
        generation: new_generation,
        envelope: proposed_envelope_id,
    };

    match entry
        .heads
        .compare_and_advance(*current_head, proposed_head)
    {
        Ok(()) => {
            info!(op_id, generation = new_generation, "publish: committed");
            send(
                ws,
                &publish_result(
                    op_id,
                    publish_ok_status(),
                    new_generation,
                    proposed_envelope_id.as_bytes(),
                    "",
                ),
            )
            .await?;
            Ok(Some(proposed_head))
        }
        Err(PublicationError::Stale { actual, .. }) => {
            info!(op_id, "publish: stale head (concurrent publish won)");
            send(
                ws,
                &publish_result(
                    op_id,
                    publish_stale_status(),
                    actual.generation,
                    actual.envelope.as_bytes(),
                    "concurrent publish won the CAS race",
                ),
            )
            .await?;
            Ok(None)
        }
        Err(e) => {
            send(
                ws,
                &publish_result(
                    op_id,
                    publish_error_status(),
                    current_head.generation,
                    current_head.envelope.as_bytes(),
                    &format!("head advance failed: {e}"),
                ),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Discover which objects are missing from the store for the proposed
/// envelope's structural closure. Iteratively walks every object the
/// server has, decodes it, discovers child references, and reports any
/// that are absent. Returns an empty vec when the full closure is present.
///
/// Bounded per call to MAX_WANT_IDS results so the client receives
/// bounded batches. The caller re-invokes after each round of uploads.
async fn discover_missing_objects(
    entry: &RepositoryEntry,
    _envelope_id: RepositoryEnvelopeId,
    envelope_oid: ObjectId,
) -> Vec<Digest256> {
    use std::collections::HashSet;

    let mut visited = HashSet::new();
    let mut frontier = vec![envelope_oid];
    let mut missing = Vec::new();

    while let Some(oid) = frontier.pop() {
        if !visited.insert(oid) {
            continue;
        }
        if missing.len() >= MAX_WANT_IDS {
            break;
        }

        match entry.objects.get(oid).await {
            Ok(obj) => {
                // Object is present — decode it and enqueue its children.
                if let Ok(children) =
                    wheat_core::extract_object_references(&obj.canonical_bytes)
                {
                    for child_id in children {
                        if !visited.contains(&child_id) {
                            frontier.push(child_id);
                        }
                    }
                }
            }
            Err(_) => {
                // Object is missing — report it.
                missing.push(Digest256 {
                    bytes: oid.as_bytes().to_vec(),
                });
            }
        }
    }

    missing
}
