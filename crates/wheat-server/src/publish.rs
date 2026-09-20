//! Publish protocol handler — receive objects, validate, commit.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

use std::collections::HashMap;

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};
use wheat_protocol::*;
use wheat_store::{
    ImmutableObjectStore, MerkleTreeStore, PublicationError, PublicationStore, RepositoryHead,
    RepositoryReader, TransitionValidationOptions, TreeDiffLimits,
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

    // Use a persistent discovery tracker that remembers visited objects
    // across rounds, so we only re-check previously-missing objects and
    // their newly-discoverable children instead of re-walking from root.
    let mut tracker = MissingObjectTracker::new(envelope_oid);
    let mut objects_complete = false;
    let missing = tracker.discover_missing(entry, proposed_envelope_id).await;
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

                // Store each object, validating identity. Track which
                // objects arrived so the tracker can re-check only those.
                let mut received_ids = Vec::with_capacity(batch.objects.len());
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
                    received_ids.push(claimed_id);
                }

                // Re-check: only the previously-missing objects that were
                // just uploaded and any new children they reference.
                tracker.mark_received(&received_ids);
                let missing = tracker.discover_missing(entry, proposed_envelope_id).await;
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

    // Walk the content set to collect all content IDs, then bulk-check
    // which ones already exist. For S3 backends this does one LIST call
    // instead of one GET per item.
    let content_set = wheat_store::ContentSetStore::new(&entry.objects);
    let mut cursor = content_set
        .cursor(manifest.content)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to walk content set: {e}")))?;
    let mut all_content_ids = Vec::new();
    while let Some(content_id) = cursor
        .next()
        .await
        .map_err(|e| ConnectionError::Internal(format!("content set walk error: {e}")))?
    {
        all_content_ids.push(content_id);
    }

    let existing = entry
        .content
        .existing_ids(&all_content_ids)
        .map_err(|e| ConnectionError::Internal(format!("content existence check failed: {e}")))?;

    let missing_content: Vec<Digest256> = all_content_ids
        .iter()
        .filter(|id| !existing.contains(id))
        .map(|id| Digest256 {
            bytes: id.as_bytes().to_vec(),
        })
        .collect();

    // Always send ContentNeeded — even with an empty list — so the client
    // doesn't hang waiting for it. An empty list means no content upload needed.
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

    if !missing_content.is_empty() {
        // Collect all content uploads first, then store them in a single
        // batch (concurrent S3 PUTs for remote backends).
        let mut content_batch: Vec<(ContentId, Vec<u8>)> = Vec::new();
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
                    content_batch.push((content_id, upload.data));
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
                        content_batch.push((content_id, chunk.data));
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

        // Store all content, with delta compression when possible.
        //
        // Diff the old and new trees to find which files changed, then use
        // the old ContentId as a delta base for each new ContentId. This
        // produces much smaller stored objects for typical edits.
        if !content_batch.is_empty() {
            let base_map =
                compute_delta_base_map(&entry.objects, &reader, current_head, &envelope_record)
                    .await;
            let with_bases: Vec<(ContentId, Vec<u8>, Option<ContentId>)> = content_batch
                .into_iter()
                .map(|(id, bytes)| {
                    let base = base_map.get(&id).copied();
                    (id, bytes, base)
                })
                .collect();
            entry
                .content
                .put_many_with_bases(with_bases, 8)
                .map_err(|e| {
                    ConnectionError::Internal(format!("failed to store content batch: {e}"))
                })?;
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
            // Seal any buffered objects into a durable batch (e.g. a packed
            // segment uploaded to S3). No-op for local stores.
            entry.objects.seal().await.map_err(|e| {
                ConnectionError::Protocol(Box::leak(
                    format!("seal after commit failed: {e}").into_boxed_str(),
                ))
            })?;
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

/// Stateful tracker for the iterative missing-object discovery during
/// publish. Remembers which objects have been visited and which were
/// reported missing, so each round only needs to re-check the
/// previously-missing objects (now received) and any new children they
/// reference — not the entire BFS from root.
struct MissingObjectTracker {
    /// All ObjectIds that have been visited (present OR reported missing).
    visited: std::collections::HashSet<ObjectId>,
    /// ObjectIds reported as missing in the most recent round. These
    /// become the starting frontier for the next round after the client
    /// sends them.
    last_missing: Vec<ObjectId>,
    /// Whether this is the first call (must start from root).
    initial: bool,
    /// Root envelope ObjectId.
    root: ObjectId,
}

impl MissingObjectTracker {
    fn new(root: ObjectId) -> Self {
        Self {
            visited: std::collections::HashSet::new(),
            last_missing: Vec::new(),
            initial: true,
            root,
        }
    }

    /// Mark received ObjectIds so they move from "missing" to "pending
    /// re-check" — their children need to be discovered.
    fn mark_received(&mut self, ids: &[ObjectId]) {
        // Remove these from visited so the next discover_missing call
        // re-visits them and discovers their children.
        for id in ids {
            self.visited.remove(id);
        }
    }

    /// Discover missing objects. On first call, walks from root. On
    /// subsequent calls, only re-checks previously-missing objects
    /// (which should now be present after the client sent them) and
    /// discovers their children.
    async fn discover_missing(
        &mut self,
        entry: &RepositoryEntry,
        envelope_id: RepositoryEnvelopeId,
    ) -> Vec<Digest256> {
        let frontier = if self.initial {
            self.initial = false;
            vec![self.root]
        } else {
            // Start from the objects that were missing last round —
            // they should now be present and we need to discover their
            // children.
            std::mem::take(&mut self.last_missing)
        };

        let mut missing = Vec::new();
        self.bfs_round(entry, frontier, &mut missing).await;

        // Walk retained sets until convergence: each round may discover
        // new trie-key ObjectIds that were missing in the previous round
        // (because their trie nodes hadn't been uploaded yet).
        while missing.len() < MAX_WANT_IDS {
            let Ok(retained_ids) =
                walk_retained_sets(&entry.objects, &entry.content, envelope_id).await
            else {
                break;
            };

            let mut new_frontier = Vec::new();
            for oid in retained_ids {
                if !self.visited.contains(&oid) {
                    new_frontier.push(oid);
                }
            }

            if new_frontier.is_empty() {
                break;
            }

            self.bfs_round(entry, new_frontier, &mut missing).await;
        }

        // Remember which objects are missing for the next round.
        self.last_missing = missing
            .iter()
            .map(|d| ObjectId::from_digest(d.bytes.as_slice().try_into().unwrap()))
            .collect();

        missing
    }

    /// Run one BFS pass from the given frontier, appending missing
    /// ObjectIds to `missing`. Updates `self.visited`.
    async fn bfs_round(
        &mut self,
        entry: &RepositoryEntry,
        mut frontier: Vec<ObjectId>,
        missing: &mut Vec<Digest256>,
    ) {
        while let Some(oid) = frontier.pop() {
            if !self.visited.insert(oid) {
                continue;
            }
            if missing.len() >= MAX_WANT_IDS {
                break;
            }

            match entry.objects.get(oid).await {
                Ok(obj) => {
                    if let Ok(children) =
                        wheat_core::extract_object_references(&obj.canonical_bytes)
                    {
                        for child_id in children {
                            if !self.visited.contains(&child_id) {
                                frontier.push(child_id);
                            }
                        }
                    }
                }
                Err(_) => {
                    missing.push(Digest256 {
                        bytes: oid.as_bytes().to_vec(),
                    });
                }
            }
        }
    }
}

/// Walk all retained sets and return the ObjectIds they contain as keys.
async fn walk_retained_sets(
    objects: &impl ImmutableObjectStore,
    content: &wheat_store::ContentStore,
    envelope_id: RepositoryEnvelopeId,
) -> Result<Vec<ObjectId>, ConnectionError> {
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(envelope_id).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read envelope for retained walk: {e}"))
    })?;
    let state = reader.state(envelope.state).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read state for retained walk: {e}"))
    })?;

    let mut retained = Vec::new();
    let store = wheat_store::RetainedSetStore::new(objects);

    for root in [
        state.retained_views,
        state.retained_revisions,
        state.retained_applications,
        state.retained_changes,
    ] {
        let mut cursor = store
            .cursor(root)
            .await
            .map_err(|e| ConnectionError::Internal(format!("retained set walk error: {e}")))?;
        while let Some(oid) = cursor
            .next()
            .await
            .map_err(|e| ConnectionError::Internal(format!("retained set cursor error: {e}")))?
        {
            retained.push(oid);
        }
    }

    Ok(retained)
}

/// Diff old and new Merkle trees to find `new_content → old_content` base
/// mappings for delta compression. Falls back to an empty map on any error
/// (delta compression is an optimization, not a correctness requirement).
#[expect(clippy::borrowed_box, reason = "MerkleTreeStore requires Sized, so &Box<dyn> is needed")]
async fn compute_delta_base_map(
    objects: &Box<dyn ImmutableObjectStore>,
    reader: &RepositoryReader<'_, Box<dyn ImmutableObjectStore>>,
    current_head: &RepositoryHead,
    proposed_envelope: &wheat_core::RepositoryEnvelopeRecord,
) -> HashMap<ContentId, ContentId> {
    let mut base_map = HashMap::new();

    // Read old tree.
    let old_envelope = match reader.envelope(current_head.envelope).await {
        Ok(e) => e,
        Err(_) => return base_map,
    };
    let old_state = match reader.state(old_envelope.state).await {
        Ok(s) => s,
        Err(_) => return base_map,
    };
    let old_view = match reader.view(old_state.base_view).await {
        Ok(v) => v,
        Err(_) => return base_map,
    };

    // Read new tree.
    let new_state = match reader.state(proposed_envelope.state).await {
        Ok(s) => s,
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
    let mut diff_cursor = trees.diff_cursor(old_view.tree, new_view.tree, TreeDiffLimits::default());
    while let Ok(Some(diff)) = diff_cursor.next().await {
        if let (Some(old_value), Some(new_value)) = (&diff.old, &diff.new)
            && old_value.content != new_value.content
        {
            base_map.insert(new_value.content, old_value.content);
        }
    }

    base_map
}
