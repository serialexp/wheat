//! Fetch protocol handler — server-driven closure send + client ObjectWant fallback.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use wheat_protocol::*;
use wheat_store::{ImmutableObjectStore, RepositoryHead};

use crate::connection::ConnectionError;
use crate::registry::RepositoryEntry;
use crate::wire::{digest_to_object_id, recv, send};

/// Handle a fetch request: compute the full structural closure and send
/// all objects proactively, then respond to any ObjectWant rounds until
/// the client sends FetchComplete.
///
/// The server uses [`wheat_store::collect_structural_closure`] to determine
/// the complete set of objects the client needs. This is domain-aware:
/// it discovers retained-set trie-key ObjectIds and per-view lineage
/// retained sets that `extract_object_references` alone cannot find.
pub async fn handle_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    head: &RepositoryHead,
    req: FetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    debug!(op_id, "fetch: starting");

    // Compute the full structural closure — every ObjectId the client needs.
    let closure = wheat_store::collect_structural_closure(
        &entry.objects,
        &entry.content,
        head.envelope,
    )
    .await;
    debug!(op_id, count = closure.len(), "fetch: closure computed");

    // Send all objects in the closure as a series of ObjectBatch messages.
    let mut batch_objects = Vec::new();
    let mut batch_size = 0usize;
    let mut sequence = 0u32;

    for oid in &closure {
        let Ok(obj) = entry.objects.get(*oid).await else {
            continue;
        };
        batch_size += obj.canonical_bytes.len();
        batch_objects.push(ObjectEntry {
            id: Digest256 {
                bytes: oid.as_bytes().to_vec(),
            },
            data: obj.canonical_bytes.to_vec(),
        });

        if batch_size >= MAX_OBJECT_BATCH_SIZE || batch_objects.len() >= MAX_OBJECT_BATCH_ITEMS {
            send(
                ws,
                &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                    message_type: 0x21,
                    op_id,
                    sequence,
                    objects: batch_objects,
                }),
            )
            .await?;
            batch_objects = Vec::new();
            batch_size = 0;
            sequence += 1;
        }
    }

    // Send any remaining objects.
    if !batch_objects.is_empty() {
        send(
            ws,
            &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                message_type: 0x21,
                op_id,
                sequence,
                objects: batch_objects,
            }),
        )
        .await?;
        sequence += 1;
    }

    // Enter the want/batch loop — the client may still request objects
    // it discovered via its own BFS that the server didn't include.
    loop {
        let msg = recv(ws).await?;
        match msg {
            WheatProtocolMessage::ObjectWant(want) => {
                if want.op_id != op_id {
                    continue;
                }
                debug!(op_id, count = want.ids.len(), "fetch: ObjectWant received");

                let mut batch_objects = Vec::new();
                let mut batch_size = 0usize;

                for digest in &want.ids {
                    let oid = digest_to_object_id(digest)?;
                    match entry.objects.get(oid).await {
                        Ok(obj) => {
                            batch_size += obj.canonical_bytes.len();
                            batch_objects.push(ObjectEntry {
                                id: digest.clone(),
                                data: obj.canonical_bytes.to_vec(),
                            });

                            // Flush if batch is getting large.
                            if batch_size >= MAX_OBJECT_BATCH_SIZE
                                || batch_objects.len() >= MAX_OBJECT_BATCH_ITEMS
                            {
                                send(
                                    ws,
                                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                                        message_type: 0x21,
                                        op_id,
                                        sequence,
                                        objects: batch_objects,
                                    }),
                                )
                                .await?;
                                batch_objects = Vec::new();
                                batch_size = 0;
                                sequence += 1;
                            }
                        }
                        Err(e) => {
                            debug!(op_id, oid = %oid, error = %e, "fetch: requested object missing");
                        }
                    }
                }

                // Always send a batch (even if empty) so the client isn't left
                // hanging when all requested objects are missing.
                send(
                    ws,
                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                        message_type: 0x21,
                        op_id,
                        sequence,
                        objects: batch_objects,
                    }),
                )
                .await?;
                sequence += 1;
            }
            WheatProtocolMessage::FetchComplete(fc) if fc.op_id == op_id => {
                debug!(op_id, "fetch: complete");
                break;
            }
            _ => {
                // Unexpected message during fetch — ignore.
            }
        }
    }

    Ok(())
}

/// Handle a content fetch request.
pub async fn handle_content_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    req: ContentFetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    debug!(op_id, count = req.content_ids.len(), "content fetch");

    let mut batch_contents = Vec::new();
    let mut batch_size = 0usize;
    let mut sequence = 0u32;

    for digest in &req.content_ids {
        let content_id = wheat_core::ContentId::from_digest(
            digest
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectionError::Protocol("invalid ContentId length"))?,
        );

        // Try to read with a bounded size first.
        match entry.content.get_bounded(content_id, MAX_CONTENT_CHUNK_SIZE) {
            Ok(data) => {
                // Fits in one batch item.
                batch_size += data.len();
                batch_contents.push(ContentEntry {
                    id: digest.clone(),
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
                match entry.content.get(content_id) {
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
                                    content_id: digest.clone(),
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
