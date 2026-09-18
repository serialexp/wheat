//! Client-side wheat native protocol engine.
//!
//! Connects to a wheat-server over WebSocket, performs session handshake,
//! and implements the client half of fetch (iterative root-first discovery)
//! and publish (object/content upload, commit).

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId, ViewId};
use wheat_protocol::*;
use wheat_store::{ContentStore, ImmutableObjectStore, RepositoryHead, RepositoryReader};

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

/// Parsed wheat remote URL.
///
/// Format: `wheat://host:port/owner/repo` (WS) or
///         `wheat+tls://host:port/owner/repo` (WSS).
#[derive(Clone, Debug)]
pub struct WheatUrl {
    /// WebSocket URL: `ws://host:port` or `wss://host:port`.
    pub ws_url: String,
    /// Repository name extracted from the URL path (e.g. `owner/repo`).
    pub repo_name: String,
}

/// Parse a `wheat://` or `wheat+tls://` URL.
pub fn parse_wheat_url(url: &str) -> Result<WheatUrl> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("wheat+tls://") {
        ("wss", rest)
    } else if let Some(rest) = url.strip_prefix("wheat://") {
        ("ws", rest)
    } else {
        bail!("invalid wheat URL: expected wheat:// or wheat+tls:// scheme");
    };

    // Split host:port from the repo path.
    let (host_port, repo_path) = rest
        .split_once('/')
        .with_context(|| format!("invalid wheat URL: no repository path in {url}"))?;

    if host_port.is_empty() {
        bail!("invalid wheat URL: empty host");
    }
    if repo_path.is_empty() {
        bail!("invalid wheat URL: empty repository name");
    }

    Ok(WheatUrl {
        ws_url: format!("{scheme}://{host_port}"),
        repo_name: repo_path.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A connected and handshaken session to a wheat-server.
pub struct WheatSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// Repository state received after opening.
pub struct RemoteRepoState {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
    pub labels: Vec<(String, ViewId)>,
}

/// Result of a publish operation.
pub enum PublishOutcome {
    Ok {
        new_generation: u64,
        new_envelope: RepositoryEnvelopeId,
    },
    Stale {
        remote_generation: u64,
        remote_envelope: RepositoryEnvelopeId,
        message: String,
    },
    Rejected {
        message: String,
    },
    Error {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Wire helpers (client-side WebSocket type differs from server-side)
// ---------------------------------------------------------------------------

async fn send_msg(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    msg: &WheatProtocolMessage,
) -> Result<()> {
    let bytes = encode_message(msg)?;
    ws.send(Message::Binary(bytes.into())).await?;
    Ok(())
}

async fn recv_msg(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<WheatProtocolMessage> {
    loop {
        let msg = ws
            .next()
            .await
            .context("connection closed unexpectedly")??;
        match msg {
            Message::Binary(bytes) => return Ok(decode_message(&bytes)?),
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await?;
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => bail!("server closed connection"),
            Message::Text(_) => bail!("unexpected text frame from server"),
        }
    }
}

fn digest_to_object_id(d: &Digest256) -> Result<ObjectId> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .context("invalid ObjectId length")?;
    Ok(ObjectId::from_digest(bytes))
}

fn digest_to_content_id(d: &Digest256) -> Result<ContentId> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .context("invalid ContentId length")?;
    Ok(ContentId::from_digest(bytes))
}

fn digest_to_envelope_id(d: &Digest256) -> Result<RepositoryEnvelopeId> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .context("invalid RepositoryEnvelopeId length")?;
    Ok(RepositoryEnvelopeId::from_digest(bytes))
}

fn digest_to_view_id(d: &Digest256) -> Result<ViewId> {
    let bytes: [u8; 32] = d
        .bytes
        .as_slice()
        .try_into()
        .context("invalid ViewId length")?;
    Ok(ViewId::from_digest(bytes))
}

fn object_id_to_digest(id: ObjectId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

fn envelope_id_to_digest(id: RepositoryEnvelopeId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

fn content_id_to_digest(id: ContentId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Connect and handshake
// ---------------------------------------------------------------------------

/// Connect to a wheat-server and perform the session handshake.
pub async fn connect(ws_url: &str) -> Result<WheatSession> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .with_context(|| format!("failed to connect to {ws_url}"))?;

    // Receive ServerHello.
    let server_hello = recv_msg(&mut ws).await?;
    match &server_hello {
        WheatProtocolMessage::ServerHello(sh) => {
            if sh.protocol_version_min > 1 || sh.protocol_version_max < 1 {
                bail!(
                    "server does not support protocol version 1 (range {}-{})",
                    sh.protocol_version_min,
                    sh.protocol_version_max
                );
            }
        }
        other => bail!("expected ServerHello, got {other:?}"),
    }

    // Send ClientHello.
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
    .await?;

    // Receive SessionEstablished.
    let established = recv_msg(&mut ws).await?;
    match &established {
        WheatProtocolMessage::SessionEstablished(se) => match &se.status {
            SessionEstablishedStatus::SessionStatusOk(_) => {}
            SessionEstablishedStatus::SessionStatusVersionMismatch(_) => {
                bail!(
                    "server rejected protocol version: {}",
                    se.message
                );
            }
            SessionEstablishedStatus::SessionStatusAuthFailed(_) => {
                bail!("authentication failed: {}", se.message);
            }
        },
        other => bail!("expected SessionEstablished, got {other:?}"),
    }

    Ok(WheatSession { ws })
}

// ---------------------------------------------------------------------------
// Repository open
// ---------------------------------------------------------------------------

/// Open a repository on the server, returning its current state.
pub async fn open_repository(
    session: &mut WheatSession,
    name: &str,
) -> Result<RemoteRepoState> {
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: name.to_string(),
        }),
    )
    .await?;

    let msg = recv_msg(&mut session.ws).await?;
    match msg {
        WheatProtocolMessage::RepositoryState(rs) => {
            let envelope = digest_to_envelope_id(&rs.head_envelope)?;
            let labels = rs
                .labels
                .iter()
                .map(|entry| {
                    let view_id = digest_to_view_id(&entry.view_id)?;
                    Ok((entry.name.clone(), view_id))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(RemoteRepoState {
                generation: rs.head_generation,
                envelope,
                labels,
            })
        }
        WheatProtocolMessage::Error(e) => {
            bail!("server error opening repository '{}': {}", name, e.message);
        }
        other => bail!("expected RepositoryState, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Fetch — iterative root-first discovery
// ---------------------------------------------------------------------------

/// Fetch all structural objects from the server using iterative
/// root-first discovery. Stores each object locally via `put_if_absent`.
///
/// After the initial BFS pass, walks the retained sets (views, revisions,
/// applications, changes) to discover objects whose IDs are encoded as
/// radix-trie keys rather than explicit node references. Those objects
/// and their transitive children are then fetched in additional rounds.
///
/// Returns the total number of objects received.
pub async fn fetch_objects<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    content: &ContentStore,
    envelope_id: RepositoryEnvelopeId,
    since_generation: u64,
    since_envelope: &[u8; 32],
) -> Result<usize> {
    // Send FetchRequest.
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::FetchRequest(FetchRequestOutput {
            message_type: 0x20,
            op_id,
            labels: vec![],
            since_generation,
            since_envelope: Digest256 {
                bytes: since_envelope.to_vec(),
            },
        }),
    )
    .await?;

    let mut total_received = 0usize;
    let mut want_list: Vec<ObjectId> = Vec::new();
    let mut known = HashSet::new();
    let mut retained_walk_done = false;

    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::ObjectBatch(batch) => {
                if batch.op_id != op_id {
                    continue;
                }

                // Store each received object, validate identity, discover children.
                for entry in &batch.objects {
                    let claimed_id = digest_to_object_id(&entry.id)?;
                    let actual_id = ObjectId::of_canonical_bytes(&entry.data);
                    if actual_id != claimed_id {
                        bail!(
                            "object identity mismatch: claimed {claimed_id}, actual {actual_id}"
                        );
                    }

                    objects.put_if_absent(claimed_id, &entry.data).await?;
                    known.insert(claimed_id);
                    total_received += 1;

                    // Discover child references.
                    if let Ok(children) =
                        wheat_core::extract_object_references(&entry.data)
                    {
                        for child_id in children {
                            if !known.contains(&child_id) {
                                // Check if we already have it locally.
                                match objects.get(child_id).await {
                                    Ok(_) => {
                                        known.insert(child_id);
                                    }
                                    Err(_) => {
                                        want_list.push(child_id);
                                    }
                                }
                            }
                        }
                    }
                }

                // Deduplicate the want list against what we now know.
                want_list.retain(|id| !known.contains(id));
                let mut seen = HashSet::new();
                want_list.retain(|id| seen.insert(*id));

                // When the BFS exhausts explicit references, walk the
                // retained sets to discover objects whose IDs are encoded
                // as radix-trie keys (not extractable from individual nodes).
                // Add discovered objects to the want list AND BFS-expand
                // any that are already present (their children may be missing).
                if want_list.is_empty() && !retained_walk_done {
                    retained_walk_done = true;
                    let retained = discover_retained_objects(objects, content, envelope_id).await?;
                    // BFS through retained objects that are already present
                    // to discover their children (revision maps, trees, etc.).
                    let mut bfs_frontier: Vec<ObjectId> = Vec::new();
                    for oid in retained {
                        if !known.contains(&oid) {
                            match objects.get(oid).await {
                                Ok(obj) => {
                                    known.insert(oid);
                                    // BFS-expand this object's children.
                                    if let Ok(children) =
                                        wheat_core::extract_object_references(&obj.canonical_bytes)
                                    {
                                        for child_id in children {
                                            if !known.contains(&child_id) {
                                                bfs_frontier.push(child_id);
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    want_list.push(oid);
                                }
                            }
                        }
                    }
                    // Continue BFS with children of retained objects.
                    while let Some(oid) = bfs_frontier.pop() {
                        if known.contains(&oid) {
                            continue;
                        }
                        match objects.get(oid).await {
                            Ok(obj) => {
                                known.insert(oid);
                                if let Ok(children) =
                                    wheat_core::extract_object_references(&obj.canonical_bytes)
                                {
                                    for child_id in children {
                                        if !known.contains(&child_id) {
                                            bfs_frontier.push(child_id);
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                want_list.push(oid);
                            }
                        }
                    }
                    // Dedup.
                    want_list.retain(|id| !known.contains(id));
                    let mut seen2 = HashSet::new();
                    want_list.retain(|id| seen2.insert(*id));
                }

                if want_list.is_empty() {
                    // We have the full closure. Send FetchComplete.
                    send_msg(
                        &mut session.ws,
                        &WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
                            message_type: 0x23,
                            op_id,
                        }),
                    )
                    .await?;
                    break;
                }

                // Request missing children (bounded).
                let batch_size = want_list.len().min(MAX_WANT_IDS);
                let want_batch: Vec<Digest256> = want_list
                    .drain(..batch_size)
                    .map(object_id_to_digest)
                    .collect();

                send_msg(
                    &mut session.ws,
                    &WheatProtocolMessage::ObjectWant(ObjectWantOutput {
                        message_type: 0x22,
                        op_id,
                        ids: want_batch,
                    }),
                )
                .await?;
            }
            other => {
                bail!("unexpected message during fetch: {other:?}");
            }
        }
    }

    Ok(total_received)
}

/// Walk the retained sets in the repository state to discover all retained
/// ObjectIds. These are encoded as radix-trie keys, not as explicit node
/// references, so `extract_object_references` cannot discover them.
async fn discover_retained_objects<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    envelope_id: RepositoryEnvelopeId,
) -> Result<Vec<ObjectId>> {
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(envelope_id).await?;
    let state = reader.state(envelope.state).await?;

    let mut retained = Vec::new();

    // Walk retained_views — keys are ViewIds (== ObjectIds).
    let views_store = wheat_store::RetainedSetStore::new(objects);
    let mut cursor = views_store.cursor(state.retained_views).await?;
    while let Some(oid) = cursor.next().await? {
        retained.push(oid);
    }

    // Walk retained_revisions — keys are RevisionIds (== ObjectIds).
    let mut cursor = views_store.cursor(state.retained_revisions).await?;
    while let Some(oid) = cursor.next().await? {
        retained.push(oid);
    }

    // Walk retained_applications — keys are ApplicationIds (== ObjectIds).
    let mut cursor = views_store.cursor(state.retained_applications).await?;
    while let Some(oid) = cursor.next().await? {
        retained.push(oid);
    }

    // Walk retained_changes — keys are ChangeObjectIds (== ObjectIds).
    let mut cursor = views_store.cursor(state.retained_changes).await?;
    while let Some(oid) = cursor.next().await? {
        retained.push(oid);
    }

    // Also walk the view lineage map — values are ViewIds.
    // And the change map — values are ChangeObjectIds.
    // These ARE extractable from nodes, but the retained sets above
    // are the critical gap.

    Ok(retained)
}

/// Fetch content from the server for all ContentIds referenced by the
/// repository's content manifest. Stores each content blob locally.
///
/// Returns the number of content blobs fetched.
pub async fn fetch_content<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    content: &ContentStore,
    envelope_id: RepositoryEnvelopeId,
) -> Result<usize> {
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(envelope_id).await?;
    let state = reader.state(envelope.state).await?;
    let manifest = reader.manifest(state.required_content).await?;

    // Walk the content set to find all ContentIds.
    let content_set = wheat_store::ContentSetStore::new(objects);
    let mut cursor = content_set.cursor(manifest.content).await?;
    let mut missing = Vec::new();
    while let Some(content_id) = cursor.next().await? {
        if !content.contains(content_id) {
            missing.push(content_id);
        }
    }

    if missing.is_empty() {
        return Ok(0);
    }

    let total = missing.len();

    // Request content in batches.
    for batch in missing.chunks(MAX_WANT_IDS) {
        let content_ids: Vec<Digest256> = batch.iter().map(|id| content_id_to_digest(*id)).collect();

        send_msg(
            &mut session.ws,
            &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
                message_type: 0x30,
                op_id,
                content_ids,
            }),
        )
        .await?;

        // Receive ContentBatch / ContentChunk messages until all content
        // in this batch is received.
        let mut received_this_batch = 0;
        let batch_len = batch.len();

        // Track chunked content being assembled.
        let mut chunk_buffer: Option<(ContentId, Vec<u8>)> = None;

        while received_this_batch < batch_len {
            let msg = recv_msg(&mut session.ws).await?;
            match msg {
                WheatProtocolMessage::ContentBatch(cb) => {
                    if cb.op_id != op_id {
                        continue;
                    }
                    for entry in &cb.contents {
                        let id = digest_to_content_id(&entry.id)?;
                        let actual = ContentId::of(&entry.data);
                        if actual != id {
                            bail!(
                                "content identity mismatch: claimed {id}, actual {actual}"
                            );
                        }
                        content.put(&entry.data)?;
                        received_this_batch += 1;
                    }
                }
                WheatProtocolMessage::ContentChunk(chunk) => {
                    if chunk.op_id != op_id {
                        continue;
                    }
                    let chunk_id = digest_to_content_id(&chunk.content_id)?;

                    // Append to the buffer.
                    match &mut chunk_buffer {
                        Some((buf_id, buf)) if *buf_id == chunk_id => {
                            buf.extend_from_slice(&chunk.data);
                        }
                        Some((old_id, _)) => {
                            bail!(
                                "interleaved content chunks: expected {old_id}, got {chunk_id}"
                            );
                        }
                        None => {
                            chunk_buffer = Some((chunk_id, chunk.data.clone()));
                        }
                    }

                    if chunk.is_final {
                        let (id, data) = chunk_buffer.take().unwrap();
                        let actual = ContentId::of(&data);
                        if actual != id {
                            bail!(
                                "chunked content identity mismatch: claimed {id}, actual {actual}"
                            );
                        }
                        content.put(&data)?;
                        received_this_batch += 1;
                    }
                }
                _ => {
                    // Ignore unexpected messages during content fetch.
                }
            }
        }
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------------

/// Publish local state to the server.
///
/// Sends the local envelope and all objects/content the server is missing,
/// then commits. Returns the outcome.
pub async fn publish<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    content: &ContentStore,
    local_envelope: RepositoryEnvelopeId,
    expected_remote: &RepositoryHead,
    force: bool,
) -> Result<PublishOutcome> {
    // Send PublishRequest.
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::PublishRequest(PublishRequestOutput {
            message_type: 0x40,
            op_id,
            publish_key: Uuid128 {
                bytes: uuid::Uuid::new_v4().as_bytes().to_vec(),
            },
            envelope_id: envelope_id_to_digest(local_envelope),
            expected_generation: expected_remote.generation,
            expected_envelope: envelope_id_to_digest(expected_remote.envelope),
            force,
        }),
    )
    .await?;

    // --- Object transfer phase ---
    // The server tells us what it needs; we send it.
    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::ObjectsNeeded(on) => {
                if on.op_id != op_id {
                    continue;
                }

                let mut batch_objects = Vec::new();
                let mut batch_size = 0usize;

                for digest in &on.ids {
                    let oid = digest_to_object_id(digest)?;
                    match objects.get(oid).await {
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
                                send_msg(
                                    &mut session.ws,
                                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                                        message_type: 0x21,
                                        op_id,
                                        sequence: 0,
                                        objects: batch_objects,
                                    }),
                                )
                                .await?;
                                batch_objects = Vec::new();
                                batch_size = 0;
                            }
                        }
                        Err(e) => {
                            bail!("server requested object {oid} which is not in local store: {e}");
                        }
                    }
                }

                // Send remaining.
                if !batch_objects.is_empty() {
                    send_msg(
                        &mut session.ws,
                        &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                            message_type: 0x21,
                            op_id,
                            sequence: 0,
                            objects: batch_objects,
                        }),
                    )
                    .await?;
                }
            }
            WheatProtocolMessage::ObjectsComplete(oc) if oc.op_id == op_id => {
                break;
            }
            WheatProtocolMessage::PublishResult(pr) if pr.op_id == op_id => {
                // Early rejection (e.g. stale head).
                return decode_publish_result(pr);
            }
            _ => {}
        }
    }

    // --- Content transfer phase ---
    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::ContentNeeded(cn) if cn.op_id == op_id => {

                for digest in &cn.content_ids {
                    let id = digest_to_content_id(digest)?;
                    let data = content.get(id)?;
                    send_msg(
                        &mut session.ws,
                        &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                            message_type: 0x44,
                            op_id,
                            content_id: digest.clone(),
                            data,
                        }),
                    )
                    .await?;
                }

                // Signal that all content has been uploaded.
                send_msg(
                    &mut session.ws,
                    &WheatProtocolMessage::ContentUploadComplete(
                        ContentUploadCompleteOutput {
                            message_type: 0x46,
                            op_id,
                        },
                    ),
                )
                .await?;

                break;
            }
            WheatProtocolMessage::PublishResult(pr) if pr.op_id == op_id => {
                return decode_publish_result(pr);
            }
            _ => {}
        }
    }

    // --- Commit phase ---
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id,
        }),
    )
    .await?;

    // Wait for PublishResult, possibly with PublishProgress in between.
    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::PublishProgress(pp) if pp.op_id == op_id => {
                // Progress updates — we could display these.
                continue;
            }
            WheatProtocolMessage::PublishResult(pr) if pr.op_id == op_id => {
                return decode_publish_result(pr);
            }
            _ => {}
        }
    }
}

fn decode_publish_result(pr: PublishResultOutput) -> Result<PublishOutcome> {
    match &pr.status {
        PublishResultStatus::PublishStatusOk(_) => {
            let envelope = digest_to_envelope_id(&pr.new_envelope)?;
            Ok(PublishOutcome::Ok {
                new_generation: pr.new_generation,
                new_envelope: envelope,
            })
        }
        PublishResultStatus::PublishStatusStaleHead(_) => {
            let envelope = digest_to_envelope_id(&pr.new_envelope)?;
            Ok(PublishOutcome::Stale {
                remote_generation: pr.new_generation,
                remote_envelope: envelope,
                message: pr.message,
            })
        }
        PublishResultStatus::PublishStatusRejected(_) => Ok(PublishOutcome::Rejected {
            message: pr.message,
        }),
        PublishResultStatus::PublishStatusAlreadyCommitted(_) => {
            let envelope = digest_to_envelope_id(&pr.new_envelope)?;
            Ok(PublishOutcome::Ok {
                new_generation: pr.new_generation,
                new_envelope: envelope,
            })
        }
        PublishResultStatus::PublishStatusError(_) => Ok(PublishOutcome::Error {
            message: pr.message,
        }),
    }
}

/// Close the session cleanly.
pub async fn close(session: &mut WheatSession) -> Result<()> {
    session.ws.close(None).await.ok();
    Ok(())
}
