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
use url::{Host, Url};

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId, ViewId};
use wheat_protocol::*;
use wheat_store::{ContentStore, ImmutableObjectStore, RepositoryHead, RepositoryReader};

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

/// Parsed and canonicalized wheat remote URL.
///
/// Format: `wheat://host:port/owner/repo` (WS) or
///         `wheat+tls://host:port/owner/repo` (WSS).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WheatUrl {
    /// `wheat` or `wheat+tls`.
    pub scheme: &'static str,
    /// Canonical `host` or `host:port` (IPv6 hosts in brackets).
    pub authority: String,
    /// WebSocket URL: `ws://host:port` or `wss://host:port`.
    pub ws_url: String,
    /// Repository name extracted from the URL path (e.g. `owner/repo`).
    pub repo_name: String,
    /// Stable remote identity. This is the canonical wheat URL, not a display URL.
    pub canonical_identity: String,
}

impl WheatUrl {
    /// Return the canonical identity used when comparing remote URLs.
    pub fn canonical_identity(&self) -> &str {
        &self.canonical_identity
    }

    #[cfg(test)]
    fn has_same_identity(&self, other: &Self) -> bool {
        self.canonical_identity == other.canonical_identity
    }
}

/// Parse a `wheat://` or `wheat+tls://` URL into a strict canonical identity.
///
/// No default port is currently part of the Wheat protocol contract, so an
/// omitted port remains omitted and every explicit port remains significant.
pub fn parse_wheat_url(input: &str) -> Result<WheatUrl> {
    // Keep the original escaped path: `url` correctly normalizes dot segments,
    // but accepting that normalization would give invalid spellings an identity.
    let after_scheme = input
        .split_once("://")
        .map(|(_, rest)| rest)
        .with_context(|| "invalid wheat URL: missing authority")?;
    let path_start = after_scheme
        .find('/')
        .with_context(|| "invalid wheat URL: no repository path")?;
    let raw_authority = &after_scheme[..path_start];
    if raw_authority.contains('@') {
        bail!("invalid wheat URL: user information is not allowed");
    }
    let raw_path = &after_scheme[path_start..];
    let raw_path = raw_path
        .split_once(['?', '#'])
        .map_or(raw_path, |(path, _)| path);

    let parsed = Url::parse(input).with_context(|| format!("invalid wheat URL: {input}"))?;
    let (wheat_scheme, ws_scheme) = match parsed.scheme() {
        "wheat" => ("wheat", "ws"),
        "wheat+tls" => ("wheat+tls", "wss"),
        _ => bail!("invalid wheat URL: expected wheat:// or wheat+tls:// scheme"),
    };

    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("invalid wheat URL: user information is not allowed");
    }
    if parsed.query().is_some() {
        bail!("invalid wheat URL: query is not allowed");
    }
    if parsed.fragment().is_some() {
        bail!("invalid wheat URL: fragment is not allowed");
    }

    let host = parsed
        .host()
        .with_context(|| "invalid wheat URL: empty host")?;
    let canonical_host = match host {
        Host::Domain(domain) => domain.to_ascii_lowercase(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    };
    let authority = match parsed.port() {
        Some(port) => format!("{canonical_host}:{port}"),
        None => canonical_host,
    };

    // Validate the original escaped path before `url` can normalize it.
    // Encoded slashes and backslashes could otherwise change repository
    // boundaries downstream.
    if contains_forbidden_path_escape(raw_path) {
        bail!("invalid wheat URL: encoded path separators or NUL are not allowed");
    }
    let raw_repo_path = raw_path
        .strip_prefix('/')
        .with_context(|| "invalid wheat URL: repository path is not absolute")?;
    let mut repo_segments = Vec::new();
    for segment in raw_repo_path.split('/') {
        let decoded = percent_decode_segment(segment)?;
        if decoded.is_empty() || decoded == "." || decoded == ".." {
            bail!("invalid wheat URL: empty or dot repository path segment");
        }
        if decoded.contains('/') || decoded.contains('\\') || decoded.contains('\0') {
            bail!("invalid wheat URL: repository path contains a separator or NUL");
        }
        repo_segments.push(decoded);
    }
    if repo_segments.is_empty() {
        bail!("invalid wheat URL: empty repository name");
    }
    let repo_name = repo_segments.join("/");
    let canonical_path = repo_segments
        .iter()
        .map(|segment| encode_identity_segment(segment))
        .collect::<Vec<_>>()
        .join("/");

    Ok(WheatUrl {
        scheme: wheat_scheme,
        ws_url: format!("{ws_scheme}://{authority}"),
        repo_name,
        canonical_identity: format!("{wheat_scheme}://{authority}/{canonical_path}"),
        authority,
    })
}

fn contains_forbidden_path_escape(path: &str) -> bool {
    path.as_bytes().windows(3).any(|bytes| {
        bytes[0] == b'%'
            && matches!(
                (bytes[1].to_ascii_lowercase(), bytes[2].to_ascii_lowercase()),
                (b'2', b'f') | (b'5', b'c') | (b'0', b'0')
            )
    })
}

fn percent_decode_segment(segment: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("invalid wheat URL: malformed percent escape in repository path");
            }
            let high = hex_value(bytes[index + 1]).with_context(
                || "invalid wheat URL: malformed percent escape in repository path",
            )?;
            let low = hex_value(bytes[index + 2]).with_context(
                || "invalid wheat URL: malformed percent escape in repository path",
            )?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("invalid wheat URL: repository path is not UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_identity_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod url_tests {
    use super::parse_wheat_url;

    #[test]
    fn canonicalizes_host_and_escapes_without_changing_repo_case() {
        let parsed = parse_wheat_url("wheat+tls://EXAMPLE.COM:443/Owner/R%C3%A9po").unwrap();
        assert_eq!(parsed.ws_url, "wss://example.com:443");
        assert_eq!(parsed.repo_name, "Owner/Répo");
        assert_eq!(
            parsed.canonical_identity(),
            "wheat+tls://example.com:443/Owner/R%C3%A9po"
        );
    }

    #[test]
    fn preserves_scheme_and_explicit_port_in_identity() {
        let plain = parse_wheat_url("wheat://example.com/owner/repo").unwrap();
        let explicit = parse_wheat_url("wheat://example.com:80/owner/repo").unwrap();
        let tls = parse_wheat_url("wheat+tls://example.com/owner/repo").unwrap();
        assert!(!plain.has_same_identity(&explicit));
        assert!(!plain.has_same_identity(&tls));
    }

    #[test]
    fn rejects_ambiguous_or_non_repository_urls() {
        for input in [
            "wheat://user@example.com/owner/repo",
            "wheat://user%40name@example.com/owner/repo",
            "wheat://example.com/owner/repo?x=1",
            "wheat://example.com/owner/repo#fragment",
            "wheat://example.com/owner//repo",
            "wheat://example.com/owner/./repo",
            "wheat://example.com/owner/../repo",
            "wheat://example.com/owner%2Frepo",
            "wheat://example.com/owner%5Crepo",
            "wheat://example.com/owner/%00repo",
        ] {
            assert!(parse_wheat_url(input).is_err(), "accepted {input}");
        }
    }
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

/// Connect to a wheat-server and perform the session handshake, sending
/// `token` (empty for none) as the bearer token.
pub async fn connect(url: &WheatUrl, token: &[u8]) -> Result<WheatSession> {
    let ws_url = url.ws_url.as_str();
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
            auth_token: token.to_vec(),
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
                bail!("server rejected protocol version: {}", se.message);
            }
            SessionEstablishedStatus::SessionStatusAuthFailed(_) if token.is_empty() => {
                bail!(
                    "{} refused the connection ({}); it needs a token: \
                     set a command that prints one with `wheat config set credential.helper <command>`",
                    url.authority,
                    se.message
                );
            }
            SessionEstablishedStatus::SessionStatusAuthFailed(_) => {
                bail!(
                    "{} refused the token from credential.helper: {}",
                    url.authority,
                    se.message
                );
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
pub async fn open_repository(session: &mut WheatSession, name: &str) -> Result<RemoteRepoState> {
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
                        bail!("object identity mismatch: claimed {claimed_id}, actual {actual_id}");
                    }

                    objects.put_if_absent(claimed_id, &entry.data).await?;
                    known.insert(claimed_id);
                    total_received += 1;

                    // Discover child references.
                    if let Ok(children) = wheat_core::extract_object_references(&entry.data) {
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
                //
                // Loop until convergence: each round may fetch objects whose
                // children include new retained-set trie nodes. Once those
                // nodes arrive, the next retained-set walk discovers additional
                // trie-key ObjectIds. Repeat until no new objects are found.
                if want_list.is_empty() {
                    loop {
                        let retained =
                            discover_retained_objects(objects, content, envelope_id).await?;
                        let mut found_new = false;

                        // BFS through retained objects that are already present
                        // to discover their children (revision maps, trees, etc.).
                        let mut bfs_frontier: Vec<ObjectId> = Vec::new();
                        for oid in retained {
                            if !known.contains(&oid) {
                                found_new = true;
                                match objects.get(oid).await {
                                    Ok(obj) => {
                                        known.insert(oid);
                                        if let Ok(children) = wheat_core::extract_object_references(
                                            &obj.canonical_bytes,
                                        ) {
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

                        // If no new retained objects were found AND no new
                        // wants were generated, convergence is reached.
                        if !found_new && want_list.is_empty() {
                            break;
                        }
                        // If we have new wants, break out to fetch them.
                        if !want_list.is_empty() {
                            break;
                        }
                    }
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
            WheatProtocolMessage::Error(e) => {
                bail!("server refused the fetch: {}", e.message);
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

    // Walk all four top-level retained sets. Best-effort: some trie nodes
    // may not be locally present yet during incremental fetch. The
    // convergence loop retries after more objects arrive.
    let retained_store = wheat_store::RetainedSetStore::new(objects);
    for root in [
        state.retained_views,
        state.retained_revisions,
        state.retained_applications,
        state.retained_changes,
    ] {
        if let Ok(mut cursor) = retained_store.cursor(root).await {
            while let Ok(Some(oid)) = cursor.next().await {
                retained.push(oid);
            }
        }
    }

    // Walk per-view lineage retained sets. The view lineage map stores,
    // for each retained parent view, a RetainedSetId root listing that
    // view's child views. These per-view retained sets also encode their
    // member ViewIds as radix-trie keys, so they need the same cursor walk.
    //
    // Best-effort: some view lineage map nodes or per-view retained-set
    // nodes may not have been fetched yet. Skip those — the convergence
    // loop will retry after more objects arrive.
    let lineage = wheat_store::ViewLineageMapStore::new(objects);
    // Collect the retained view IDs first (they're already in `retained`).
    let retained_view_ids: Vec<wheat_core::ViewId> = {
        let mut ids = Vec::new();
        // Re-walk; the cursor is consumed.
        if let Ok(mut cursor) = retained_store.cursor(state.retained_views).await {
            while let Ok(Some(oid)) = cursor.next().await {
                ids.push(wheat_core::ViewId::from_digest(*oid.as_bytes()));
            }
        }
        ids
    };
    for view_id in &retained_view_ids {
        if let Ok(Some(children_root)) = lineage.get(state.view_lineage, *view_id).await
            && let Ok(mut cursor) = retained_store.cursor(children_root).await
        {
            while let Ok(Some(oid)) = cursor.next().await {
                retained.push(oid);
            }
        }
    }

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
        let content_ids: Vec<Digest256> =
            batch.iter().map(|id| content_id_to_digest(*id)).collect();

        send_msg(
            &mut session.ws,
            &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
                message_type: 0x30,
                op_id,
                content_ids,
            }),
        )
        .await?;

        // Receive ContentBatch / ContentChunk messages until every blob in
        // this batch has arrived. Only the requested blobs are accepted, each
        // once, chunk by chunk in order and no larger than the store takes.
        let mut receiver = ContentReceiver::new(batch.iter().copied(), MAX_CONTENT_SIZE);

        while !receiver.is_complete() {
            let msg = recv_msg(&mut session.ws).await?;
            match msg {
                WheatProtocolMessage::ContentBatch(cb) => {
                    if cb.op_id != op_id {
                        continue;
                    }
                    for entry in &cb.contents {
                        let id = digest_to_content_id(&entry.id)?;
                        receiver
                            .accept_whole(id, entry.data.len())
                            .context("server sent content that breaks the fetch rules")?;
                        let actual = ContentId::of(&entry.data);
                        if actual != id {
                            bail!("content identity mismatch: claimed {id}, actual {actual}");
                        }
                        content.put(&entry.data)?;
                    }
                }
                WheatProtocolMessage::ContentChunk(chunk) => {
                    if chunk.op_id != op_id {
                        continue;
                    }
                    let chunk_id = digest_to_content_id(&chunk.content_id)?;
                    let finished = receiver
                        .accept_chunk(chunk_id, chunk.offset, chunk.data, chunk.is_final)
                        .context("server sent content that breaks the fetch rules")?;
                    if let Some((id, data)) = finished {
                        let actual = ContentId::of(&data);
                        if actual != id {
                            bail!(
                                "chunked content identity mismatch: claimed {id}, actual {actual}"
                            );
                        }
                        content.put(&data)?;
                    }
                }
                WheatProtocolMessage::Error(e) if e.op_id == op_id => {
                    bail!("server refused the content fetch: {}", e.message);
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
                    upload_content(session, op_id, digest, data).await?;
                }

                // Signal that all content has been uploaded.
                send_msg(
                    &mut session.ws,
                    &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                        message_type: 0x46,
                        op_id,
                    }),
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

/// Send one requested blob: in one `ContentUpload` when it fits a chunk,
/// otherwise as `ContentUploadChunk` messages of at most
/// `MAX_CONTENT_CHUNK_SIZE` bytes each, so no message passes the size limit.
async fn upload_content(
    session: &mut WheatSession,
    op_id: u32,
    digest: &Digest256,
    data: Vec<u8>,
) -> Result<()> {
    if data.len() <= MAX_CONTENT_CHUNK_SIZE {
        return send_msg(
            &mut session.ws,
            &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                message_type: 0x44,
                op_id,
                content_id: digest.clone(),
                data,
            }),
        )
        .await;
    }
    let mut offset = 0;
    for piece in data.chunks(MAX_CONTENT_CHUNK_SIZE) {
        let end = offset + piece.len();
        send_msg(
            &mut session.ws,
            &WheatProtocolMessage::ContentUploadChunk(ContentUploadChunkOutput {
                message_type: 0x45,
                op_id,
                content_id: digest.clone(),
                offset: offset as u64,
                data: piece.to_vec(),
                is_final: end == data.len(),
            }),
        )
        .await?;
        offset = end;
    }
    Ok(())
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
