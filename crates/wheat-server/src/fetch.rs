//! Fetch protocol handler — iterative root-first discovery.

use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use wheat_core::ObjectId;
use wheat_protocol::*;
use wheat_store::{ImmutableObjectStore, RepositoryHead, RepositoryReader};

use crate::connection::ConnectionError;
use crate::registry::RepositoryEntry;
use crate::wire::{digest_to_object_id, recv, send};

/// Handle a fetch request: send the envelope and top-level objects, then
/// respond to ObjectWant rounds until the client sends FetchComplete.
pub async fn handle_fetch(
    ws: &mut WebSocketStream<TcpStream>,
    entry: &RepositoryEntry,
    head: &RepositoryHead,
    req: FetchRequestOutput,
) -> Result<(), ConnectionError> {
    let op_id = req.op_id;
    debug!(op_id, "fetch: starting");

    // Send the envelope object as the first batch.
    let envelope_oid: ObjectId = head.envelope.into();
    let envelope_obj = entry
        .objects
        .get(envelope_oid)
        .await
        .map_err(|e| ConnectionError::Internal(format!("missing envelope object: {e}")))?;

    // Also read the state object so we send two in the first batch.
    let reader = RepositoryReader::new(&entry.objects, &entry.content);
    let envelope_record = reader
        .envelope(head.envelope)
        .await
        .map_err(|e| ConnectionError::Internal(format!("failed to read envelope: {e}")))?;

    let state_oid: ObjectId = envelope_record.state.into();
    let state_obj = entry
        .objects
        .get(state_oid)
        .await
        .map_err(|e| ConnectionError::Internal(format!("missing state object: {e}")))?;

    // Send initial batch with envelope + state.
    let mut objects = vec![
        ObjectEntry {
            id: Digest256 {
                bytes: envelope_oid.as_bytes().to_vec(),
            },
            data: envelope_obj.canonical_bytes.to_vec(),
        },
        ObjectEntry {
            id: Digest256 {
                bytes: state_oid.as_bytes().to_vec(),
            },
            data: state_obj.canonical_bytes.to_vec(),
        },
    ];

    // Also send the labels manifest.
    let labels_oid: ObjectId = envelope_record.labels.into();
    if let Ok(labels_obj) = entry.objects.get(labels_oid).await {
        objects.push(ObjectEntry {
            id: Digest256 {
                bytes: labels_oid.as_bytes().to_vec(),
            },
            data: labels_obj.canonical_bytes.to_vec(),
        });
    }

    send(
        ws,
        &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
            message_type: 0x21,
            op_id,
            sequence: 0,
            objects,
        }),
    )
    .await?;

    // Enter the want/batch loop.
    let mut sequence = 1;
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
