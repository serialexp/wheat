//! Client-side wheat native protocol engine.
//!
//! Connects to a wheat-server over WebSocket, performs session handshake,
//! and implements the client half of fetch (the server pushes what the client's
//! last fetch lacks, the client pulls any gaps) and publish (object/content
//! upload, commit).

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::{Host, Url};

use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId, ViewId};
use wheat_protocol::*;
use wheat_store::{
    ContentStore, DeltaStep, ImmutableObjectStore, RepositoryHead, RepositoryReader,
    RepositoryStage, RepositoryStore, StructuralDelta, StructuralDeltaLimits,
};

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
    // Refuse an oversized message while it arrives, not after it is in memory.
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    // Nagle's algorithm would hold back the tail of a message until the server's
    // delayed ACK, which costs every request/reply round tens of milliseconds.
    let disable_nagle = true;
    let (mut ws, _) = connect_async_with_config(ws_url, Some(limits), disable_nagle)
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
// Fetch — the server pushes what `since` lacks, the client pulls any gaps
// ---------------------------------------------------------------------------

/// Writers used to store one batch of fetched objects.
const FETCH_WRITE_CONCURRENCY: usize = 8;

/// Most objects a fetch or clone stages before writing them early.
const STAGED_OBJECTS: usize = 65_536;
/// Most object bytes, and separately most content bytes, a fetch or clone stages
/// before writing them early, so it never holds every blob in memory at once.
const STAGED_BYTES: usize = 64 * 1024 * 1024;

/// A write buffer over `store` for one fetch or clone. Each durable write costs
/// several fsyncs, so the objects and content it receives, and the objects a merge
/// with local work makes, are stored together by [`RepositoryStage::flush`] just
/// before the head moves, not once per message. Reads see what is staged. Only a
/// transfer larger than the stage is written early, in stage-sized batches.
pub fn staged<S: RepositoryStore + ?Sized>(store: &S) -> RepositoryStage<'_, S> {
    RepositoryStage::new(store, STAGED_OBJECTS, STAGED_BYTES)
}

/// What one structural fetch transferred.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FetchedObjects {
    /// Objects the server pushed.
    pub pushed: u64,
    /// Objects the client asked for afterwards.
    pub pulled: u64,
    /// `ObjectWant` round trips.
    pub wants: u32,
    /// Whether the server left out what `since` covers.
    pub incremental: bool,
}

/// Fetch the structural objects of `want` into `objects`.
///
/// The server pushes the objects that `since` does not cover (everything when
/// `since` is `None` or unknown to it). The client then runs the same walk over
/// its own store ([`StructuralDelta`]) and asks for whatever is still missing, so
/// it ends with the whole closure of `want` whatever the server pushed. `since`
/// should be an envelope whose closure the local store holds; where it does not,
/// the walk asks for more.
pub async fn fetch_objects<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    want: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
) -> Result<FetchedObjects> {
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::FetchRequest(FetchRequestOutput {
            message_type: 0x20,
            op_id,
            want_envelope: envelope_id_to_digest(want),
            since_envelope: since.map_or(Digest256 { bytes: vec![0; 32] }, envelope_id_to_digest),
        }),
    )
    .await?;

    let mut fetched = FetchedObjects::default();
    let mut receiver = TransferReceiver::unrequested(MAX_OBJECT_SIZE);
    loop {
        match recv_msg(&mut session.ws).await? {
            WheatProtocolMessage::ObjectBatch(batch) if batch.op_id == op_id => {
                fetched.pushed += store_whole(objects, &mut receiver, batch.objects).await?;
            }
            WheatProtocolMessage::ObjectChunk(chunk) if chunk.op_id == op_id => {
                if store_chunk(objects, &mut receiver, chunk).await? {
                    fetched.pushed += 1;
                }
            }
            WheatProtocolMessage::FetchPushEnd(end) if end.op_id == op_id => {
                receiver
                    .finish()
                    .context("server ended the push inside a chunked object")?;
                if end.pushed_objects != fetched.pushed {
                    bail!(
                        "server says it pushed {} objects, but {} arrived",
                        end.pushed_objects,
                        fetched.pushed
                    );
                }
                fetched.incremental = end.incremental;
                break;
            }
            WheatProtocolMessage::Error(e) if e.op_id == op_id => {
                bail!("server refused the fetch: {}", e.message);
            }
            other => bail!("unexpected message during fetch: {other:?}"),
        }
    }

    let mut delta = StructuralDelta::new(objects, since, want, StructuralDeltaLimits::default());
    while let Some(step) = delta.next().await? {
        let DeltaStep::Missing(ids) = step else {
            continue;
        };
        fetched.pulled += pull(session, op_id, objects, fetched.wants, &ids).await?;
        fetched.wants += 1;
        delta.resume();
    }

    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
            message_type: 0x23,
            op_id,
        }),
    )
    .await?;
    Ok(fetched)
}

/// Checks and stores objects that arrived whole. Returns how many.
async fn store_whole<O: ImmutableObjectStore>(
    objects: &O,
    receiver: &mut TransferReceiver<ObjectId>,
    entries: Vec<ObjectEntry>,
) -> Result<u64> {
    let mut batch = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = digest_to_object_id(&entry.id)?;
        receiver
            .accept_whole(id, entry.data.len())
            .context("server sent objects that break the fetch rules")?;
        check_object_id(id, &entry.data)?;
        batch.push((id, entry.data));
    }
    objects.put_batch(&batch, FETCH_WRITE_CONCURRENCY).await?;
    Ok(batch.len() as u64)
}

/// Takes one chunk; checks and stores the object after its last. Returns whether
/// an object was finished.
async fn store_chunk<O: ImmutableObjectStore>(
    objects: &O,
    receiver: &mut TransferReceiver<ObjectId>,
    chunk: ObjectChunkOutput,
) -> Result<bool> {
    let id = digest_to_object_id(&chunk.object_id)?;
    let Some((id, data)) = receiver
        .accept_chunk(id, chunk.offset, chunk.data, chunk.is_final)
        .context("server sent object chunks that break the fetch rules")?
    else {
        return Ok(false);
    };
    check_object_id(id, &data)?;
    objects.put_if_absent(id, &data).await?;
    Ok(true)
}

fn check_object_id(id: ObjectId, data: &[u8]) -> Result<()> {
    let actual = ObjectId::of_canonical_bytes(data);
    if actual != id {
        bail!("object identity mismatch: claimed {id}, actual {actual}");
    }
    Ok(())
}

/// Sends one `ObjectWant` and stores its one reply. Returns how many objects
/// arrived; the server may answer only the front of the list.
async fn pull<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    want_sequence: u32,
    ids: &[ObjectId],
) -> Result<u64> {
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::ObjectWant(ObjectWantOutput {
            message_type: 0x22,
            op_id,
            want_sequence,
            ids: ids.iter().copied().map(object_id_to_digest).collect(),
        }),
    )
    .await?;
    let mut receiver = TransferReceiver::new(ids.iter().copied(), MAX_OBJECT_SIZE);
    let mut arrived = 0;
    loop {
        match recv_msg(&mut session.ws).await? {
            WheatProtocolMessage::ObjectChunk(chunk) if chunk.op_id == op_id => {
                if store_chunk(objects, &mut receiver, chunk).await? {
                    arrived += 1;
                }
            }
            WheatProtocolMessage::ObjectWantReply(reply) if reply.op_id == op_id => {
                if reply.want_sequence != want_sequence {
                    bail!(
                        "server answered want {} while want {want_sequence} was open",
                        reply.want_sequence
                    );
                }
                arrived += store_whole(objects, &mut receiver, reply.objects).await?;
                receiver
                    .finish_partial()
                    .context("server ended a want reply inside a chunked object")?;
                if arrived == 0 {
                    bail!("server answered a want for {} objects with none", ids.len());
                }
                return Ok(arrived);
            }
            WheatProtocolMessage::Error(e) if e.op_id == op_id => {
                bail!("server refused the fetch: {}", e.message);
            }
            other => bail!("unexpected message during fetch: {other:?}"),
        }
    }
}

/// What one content fetch transferred.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FetchedContent {
    /// Content IDs looked up in the local store.
    pub checked: usize,
    /// Blobs fetched from the server.
    pub fetched: usize,
}

/// Fetch the content that `envelope_id`'s aggregate content set requires and the
/// local store lacks.
///
/// With `since`, only content that `since`'s set lacks is looked up: the
/// client keeps an envelope as `since` only once all its content is stored.
/// Fetched content is staged with the objects in `stage`.
pub async fn fetch_content<S: RepositoryStore + ?Sized>(
    session: &mut WheatSession,
    op_id: u32,
    stage: &RepositoryStage<'_, S>,
    envelope_id: RepositoryEnvelopeId,
    since: Option<RepositoryEnvelopeId>,
) -> Result<FetchedContent> {
    let content = stage.content();
    let reader = RepositoryReader::new(stage, content);
    let required = async |envelope_id| -> Result<_> {
        let envelope = reader.envelope(envelope_id).await?;
        let state = reader.state(envelope.state).await?;
        Ok(state.required_content)
    };
    let new = required(envelope_id).await?;
    let content_set = wheat_store::ContentSetStore::new(stage);
    let candidates = match since {
        Some(since) => content_set.added(required(since).await?, new).await?,
        None => {
            let mut all = Vec::new();
            let mut cursor = content_set.cursor(new).await?;
            while let Some(id) = cursor.next().await? {
                all.push(id);
            }
            all
        }
    };
    let missing = candidates
        .iter()
        .copied()
        .filter(|id| !content.contains(*id))
        .collect::<Vec<_>>();
    let checked = candidates.len();

    if missing.is_empty() {
        return Ok(FetchedContent {
            checked,
            fetched: 0,
        });
    }

    let total = missing.len();
    // The stage checks that each blob matches its ID.
    let stage_blob = async |id, data| {
        stage
            .put_content([(id, data, None)])
            .await
            .context("storing fetched content")
    };

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
        let mut receiver = TransferReceiver::new(batch.iter().copied(), MAX_CONTENT_SIZE);

        while !receiver.is_complete() {
            let msg = recv_msg(&mut session.ws).await?;
            match msg {
                WheatProtocolMessage::ContentBatch(cb) => {
                    if cb.op_id != op_id {
                        continue;
                    }
                    for entry in cb.contents {
                        let id = digest_to_content_id(&entry.id)?;
                        receiver
                            .accept_whole(id, entry.data.len())
                            .context("server sent content that breaks the fetch rules")?;
                        stage_blob(id, entry.data).await?;
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
                        stage_blob(id, data).await?;
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

    Ok(FetchedContent {
        checked,
        fetched: total,
    })
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
    push_difference(
        session,
        op_id,
        objects,
        content,
        local_envelope,
        expected_remote.envelope,
    )
    .await?;

    // --- Object transfer phase ---
    // The server asks in rounds for what the push left out; answer each with
    // exactly the objects it names.
    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::ObjectsNeeded(on) if on.op_id == op_id => {
                upload_objects(session, op_id, objects, &on.ids).await?;
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
    // The server asks in rounds; answer each, until it says it has everything.
    loop {
        let msg = recv_msg(&mut session.ws).await?;
        match msg {
            WheatProtocolMessage::ContentNeeded(cn) if cn.op_id == op_id => {
                for digest in &cn.content_ids {
                    let id = digest_to_content_id(digest)?;
                    let data = content.get(id)?;
                    upload_content(session, op_id, digest, data).await?;
                }

                // Signal that this round's content has been uploaded.
                send_msg(
                    &mut session.ws,
                    &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                        message_type: 0x46,
                        op_id,
                    }),
                )
                .await?;
            }
            WheatProtocolMessage::ContentComplete(cc) if cc.op_id == op_id => break,
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

/// What one up-front push sent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PushedDifference {
    objects: u32,
    contents: u32,
    bytes: usize,
}

/// Pushes, unasked, what the server needs for `local` if its head is `expected`, so
/// a publish needs no round of the server's walk: the objects of the walk from
/// `expected` to `local`, then the added blobs of at most [`MAX_PUSHED_BLOB_SIZE`]
/// bytes, together at most [`MAX_PUSHED_BYTES`], then `PublishPushEnd`. The server
/// asks for what this leaves out: anything past the limits, and what its head lacks
/// when it has moved past `expected`. Without `expected` here, the walk starts
/// from nothing and stops at the limit.
#[tracing::instrument(name = "publish_push", level = "debug", skip_all)]
async fn push_difference<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    content: &ContentStore,
    local: RepositoryEnvelopeId,
    expected: RepositoryEnvelopeId,
) -> Result<PushedDifference> {
    let expected_object = ObjectId::from_digest(*expected.as_bytes());
    let since = (*expected.as_bytes() != [0; 32]
        && objects.exists_many(&[expected_object]).await? == [true])
    .then_some(expected);
    let mut pushed = PushedDifference::default();

    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut delta = StructuralDelta::new(objects, since, local, StructuralDeltaLimits::default());
    while let Some(step) = delta.next().await? {
        let object = match step {
            DeltaStep::Object(object) => object,
            // This store lacks part of its own head. The server asks for it, and
            // answering says what is missing.
            DeltaStep::Missing(_) => break,
        };
        let len = object.canonical_bytes.len();
        // An object too large for a batch is asked for and sent in chunks.
        if len > MAX_OBJECT_BATCH_SIZE {
            continue;
        }
        if pushed.bytes + len > MAX_PUSHED_BYTES {
            break;
        }
        if batch.len() == MAX_OBJECT_BATCH_ITEMS || batch_bytes + len > MAX_OBJECT_BATCH_SIZE {
            send_object_batch(session, op_id, std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
        batch_bytes += len;
        pushed.bytes += len;
        pushed.objects += 1;
        batch.push(ObjectEntry {
            id: object_id_to_digest(object.id),
            data: object.canonical_bytes.to_vec(),
        });
    }
    if !batch.is_empty() {
        send_object_batch(session, op_id, batch).await?;
    }

    let reader = RepositoryReader::new(objects, content);
    let required = async |envelope_id| -> Result<_> {
        let envelope = reader.envelope(envelope_id).await?;
        Ok(reader.state(envelope.state).await?.required_content)
    };
    let sets = wheat_store::ContentSetStore::new(objects);
    match since {
        Some(since) => {
            for id in sets
                .added(required(since).await?, required(local).await?)
                .await?
            {
                if !push_blob(session, op_id, content, id, &mut pushed).await? {
                    break;
                }
            }
        }
        // A first publish: every blob is new, so stop reading at the limit.
        None => {
            let mut cursor = sets.cursor(required(local).await?).await?;
            while let Some(id) = cursor.next().await? {
                if !push_blob(session, op_id, content, id, &mut pushed).await? {
                    break;
                }
            }
        }
    }

    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::PublishPushEnd(PublishPushEndOutput {
            message_type: 0x4b,
            op_id,
            pushed_objects: pushed.objects,
            pushed_contents: pushed.contents,
        }),
    )
    .await?;
    Ok(pushed)
}

/// Pushes blob `id` when it is small enough and fits what is left of the push.
/// Returns whether more may follow.
async fn push_blob(
    session: &mut WheatSession,
    op_id: u32,
    content: &ContentStore,
    id: ContentId,
    pushed: &mut PushedDifference,
) -> Result<bool> {
    let left = MAX_PUSHED_BYTES - pushed.bytes;
    if left == 0 {
        return Ok(false);
    }
    // The stored length is read from metadata, so a large blob is never read here.
    let limit = MAX_PUSHED_BLOB_SIZE.min(left);
    if content.logical_length(id)? > limit as u64 {
        return Ok(true);
    }
    let data = content.get_bounded(id, limit)?;
    pushed.bytes += data.len();
    pushed.contents += 1;
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
            message_type: 0x44,
            op_id,
            content_id: content_id_to_digest(id),
            data,
        }),
    )
    .await?;
    Ok(true)
}

/// Send the objects of one `ObjectsNeeded`: in `ObjectBatch`es that stay within
/// the batch limits, and each object too large for a batch as `ObjectChunk`s.
async fn upload_objects<O: ImmutableObjectStore>(
    session: &mut WheatSession,
    op_id: u32,
    objects: &O,
    ids: &[Digest256],
) -> Result<()> {
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    for digest in ids {
        let id = digest_to_object_id(digest)?;
        let object = objects
            .get(id)
            .await
            .with_context(|| format!("server asked for object {id}, which is not here"))?;
        let data = object.canonical_bytes;
        if data.len() > MAX_OBJECT_BATCH_SIZE {
            for chunk in object_chunks(op_id, id, &data) {
                send_msg(&mut session.ws, &chunk).await?;
            }
            continue;
        }
        // Send first, so a batch never grows past its limits.
        if batch.len() == MAX_OBJECT_BATCH_ITEMS || batch_bytes + data.len() > MAX_OBJECT_BATCH_SIZE
        {
            send_object_batch(session, op_id, std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
        batch_bytes += data.len();
        batch.push(ObjectEntry {
            id: digest.clone(),
            data: data.to_vec(),
        });
    }
    if !batch.is_empty() {
        send_object_batch(session, op_id, batch).await?;
    }
    Ok(())
}

async fn send_object_batch(
    session: &mut WheatSession,
    op_id: u32,
    objects: Vec<ObjectEntry>,
) -> Result<()> {
    send_msg(
        &mut session.ws,
        &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
            message_type: 0x21,
            op_id,
            sequence: 0,
            objects,
        }),
    )
    .await
}

/// One object as `ObjectChunk` messages of at most `MAX_OBJECT_CHUNK_SIZE` bytes.
fn object_chunks(
    op_id: u32,
    id: ObjectId,
    data: &[u8],
) -> impl Iterator<Item = WheatProtocolMessage> + '_ {
    let mut offset = 0;
    data.chunks(MAX_OBJECT_CHUNK_SIZE).map(move |piece| {
        let start = offset;
        offset += piece.len();
        WheatProtocolMessage::ObjectChunk(ObjectChunkOutput {
            message_type: 0x26,
            op_id,
            object_id: object_id_to_digest(id),
            offset: start as u64,
            data: piece.to_vec(),
            is_final: offset == data.len(),
        })
    })
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

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn an_object_splits_into_chunks_the_receiver_puts_back_together() {
        let data: Vec<u8> = (0..2 * MAX_OBJECT_CHUNK_SIZE + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let id = ObjectId::of_canonical_bytes(&data);
        let mut receiver = TransferReceiver::new([id], MAX_OBJECT_SIZE);
        let mut chunks = 0;
        let mut finished = None;
        for message in object_chunks(9, id, &data) {
            let WheatProtocolMessage::ObjectChunk(chunk) = message else {
                panic!("expected ObjectChunk");
            };
            assert_eq!(chunk.op_id, 9);
            assert!(chunk.data.len() <= MAX_OBJECT_CHUNK_SIZE);
            assert!(encode_message(&WheatProtocolMessage::ObjectChunk(chunk.clone())).is_ok());
            chunks += 1;
            finished = receiver
                .accept_chunk(
                    digest_to_object_id(&chunk.object_id).unwrap(),
                    chunk.offset,
                    chunk.data,
                    chunk.is_final,
                )
                .unwrap();
        }
        assert_eq!(chunks, 3);
        assert_eq!(finished, Some((id, data)));
        receiver.finish().unwrap();
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn a_message_over_the_limit_is_refused_before_it_ends() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = parse_wheat_url(&format!(
            "wheat://{}/owner/repo",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        // A server that starts an unfinished binary message: a first fragment as
        // large as a message may be, then a one-byte continuation, then nothing.
        // Each frame is within the frame limit; only a message limit at
        // MAX_MESSAGE_SIZE refuses the message now.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let stream = ws.get_mut();
            let mut frames = vec![0x02, 127];
            frames.extend_from_slice(&(MAX_MESSAGE_SIZE as u64).to_be_bytes());
            frames.resize(frames.len() + MAX_MESSAGE_SIZE, 0);
            frames.extend_from_slice(&[0x00, 1, 0]);
            stream.write_all(&frames).await.unwrap();
            stream.flush().await.unwrap();
            // Keep the connection open, so only the limit can end the read.
            std::future::pending::<()>().await;
        });

        // A generous bound: this asserts the client stops waiting, not how fast.
        let connected =
            tokio::time::timeout(std::time::Duration::from_secs(30), connect(&url, b"")).await;
        server.abort();
        let Ok(connected) = connected else {
            panic!("the client kept waiting for the rest of the message");
        };
        assert!(connected.is_err(), "the oversized message was accepted");
    }
}
