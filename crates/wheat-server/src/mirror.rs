//! Mirroring a Wheat remote into a served repository. The remote is authoritative:
//! [`mirror`] makes the repository's head the remote's envelope, labels and all,
//! replacing whatever the repository held, as a forced publish naming the current
//! head would. Only with the `mirror` feature.
//!
//! The remote's objects and content are fetched into a stage over the network's store
//! (incrementally from the current head, which the last mirror left at the remote's
//! envelope then), and the move joins the repository's group commit as a forced
//! publish naming the current head does: the content it relies on named first when
//! the network is told that ([`crate::naming`]), and made durable in its batch. No
//! publish hooks run: the embedder tells its own listeners.
//!
//! What arrived is validated before it joins: only what it adds, when the remote's
//! history extends the current head's (the usual case: the remote moved on since the
//! last mirror), and whole otherwise (the remote's history was rewritten, or the
//! repository was published to meanwhile), as a forced fetch is.
//!
//! The mirror holds a slot among the repository's writers from before it reads the
//! head until the head has moved, so a quiesce (an export, a collection) waits for
//! the whole fetch: the fetched objects are in the pool before a head names them.

use std::{future::Future, sync::Arc};

use tokio::time::Instant;
use tracing::{Instrument as _, debug_span, warn};
use wheat_core::{RepositoryEnvelopeId, publish_request_digest};
use wheat_remote::{FetchedContent, FetchedObjects, RemoteRepoState, WheatSession, WheatUrl};
use wheat_store::{Arrival, RepositoryHead, RepositoryReader, RepositoryStage};

use crate::{
    access::Principal,
    commit::{self, CommitFailure},
    error_text::chain,
    naming::Named,
    network::Network,
    receipt::PublishKey,
    registry::RepositoryEntry,
    write::{self, WriteFailure},
};

/// What [`mirror`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mirrored {
    /// The repository's head, at the remote's envelope.
    pub head: RepositoryHead,
    /// Whether the head moved: false when it was at the remote's envelope already,
    /// and nothing was fetched.
    pub moved: bool,
    /// What the structural fetch transferred.
    pub objects: FetchedObjects,
    /// What the content fetch transferred.
    pub content: FetchedContent,
}

/// Why [`mirror`] did not move the head. The head is as it was.
#[derive(Debug)]
pub enum MirrorFailure {
    /// The remote could not be reached, refused, or broke the protocol.
    Remote(String),
    /// The head could not be moved. [`CommitFailure::Stale`] when a publish to the
    /// repository landed while the mirror fetched; mirroring again replaces it.
    /// [`CommitFailure::Rejected`] when what the remote sent is not a valid
    /// repository. [`WriteFailure::Quiesced`] when the network refuses writes for the
    /// moment.
    Write(WriteFailure),
}

impl From<CommitFailure> for MirrorFailure {
    fn from(failure: CommitFailure) -> Self {
        Self::Write(WriteFailure::Commit(failure))
    }
}

/// Makes the head of the repository `entry` the head of the Wheat remote `url`,
/// connecting with `token` (empty for none) as a client would. `principal` is who
/// the move is recorded for in the head's receipts.
///
/// Wheat resolves `url`'s host name and connects to whatever it answers. For a URL
/// a user gave, use [`mirror_over`] and connect yourself.
pub async fn mirror(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    url: &WheatUrl,
    token: &[u8],
) -> Result<Mirrored, MirrorFailure> {
    let (url, token) = (url.clone(), token.to_vec());
    mirror_opening(entry, principal, move || async move {
        wheat_remote::open(&url, &token).await
    })
    .await
}

/// [`mirror`] over a connection `connect` makes to the server `url` names: a host
/// mirroring addresses users give it resolves and vets the name itself and connects
/// only to the address it vetted, so a name that resolves again to somewhere else
/// (DNS rebinding) is never used. Wheat never resolves `url`; it runs TLS for
/// `wheat+tls://` over the connection, checking the certificate against `url`'s host
/// name, and the rest of the session (see [`wheat_remote::open_over`]). A connection
/// `connect` refuses is [`MirrorFailure::Remote`] with its error.
///
/// `connect` is given `url` and runs, with the fetch, on a thread for blocking work.
pub async fn mirror_over<C, Fut, S>(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    url: &WheatUrl,
    token: &[u8],
    connect: C,
) -> Result<Mirrored, MirrorFailure>
where
    C: FnOnce(WheatUrl) -> Fut + Send + 'static,
    Fut: Future<Output = std::io::Result<S>>,
    S: wheat_remote::RemoteStream,
{
    use anyhow::Context as _;

    let (url, token) = (url.clone(), token.to_vec());
    mirror_opening(entry, principal, move || async move {
        let stream = connect(url.clone())
            .await
            .with_context(|| format!("failed to connect to {}", url.ws_url))?;
        wheat_remote::open_over(stream, &url, &token).await
    })
    .await
}

/// [`mirror`], with the session to the remote `open` opens.
#[tracing::instrument(name = "server_mirror", level = "debug", skip_all)]
async fn mirror_opening<O, Fut>(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    open: O,
) -> Result<Mirrored, MirrorFailure>
where
    O: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<(WheatSession, RemoteRepoState)>>,
{
    // Held until the head has moved, so a quiesce waits for the fetch (see the
    // module's documentation).
    let writing = write::begin(entry).map_err(MirrorFailure::Write)?;
    // Before anything is fetched: each stamp of the fetched content is newer.
    let opened = Instant::now();
    let current =
        entry.heads.head().await.map_err(|e| {
            CommitFailure::Internal(format!("mirror: read the head: {}", chain(&e)))
        })?;
    let staged = Arc::new(RepositoryStage::new(
        entry.network.clone(),
        write::STAGE_OBJECTS,
        write::STAGE_BYTES,
    ));
    let fetched = {
        let staged = Arc::clone(&staged);
        // The stores read and write the disk synchronously, so the fetch runs on a
        // thread for blocking work.
        crate::blocking::drive(move || async move { fetch(&staged, open, current.envelope).await })
            .await
            .map_err(|e| CommitFailure::Internal(format!("mirror: the fetch stopped: {e}")))?
            .map_err(|e| MirrorFailure::Remote(format!("{e:#}")))?
    };
    let Some((remote, objects, content)) = fetched else {
        return Ok(Mirrored {
            head: current,
            moved: false,
            objects: FetchedObjects::default(),
            content: FetchedContent::default(),
        });
    };

    {
        let staged = Arc::clone(&staged);
        crate::blocking::drive(
            move || async move { validate(&staged, current.envelope, remote).await },
        )
        .await
        .map_err(|e| CommitFailure::Internal(format!("mirror: validation stopped: {e}")))?
        .map_err(|message| CommitFailure::Rejected {
            head: current,
            message: format!("the remote's head failed validation: {message}"),
        })?;
    }

    let named = match &entry.network.naming {
        None => None,
        Some(naming) => {
            let needed = {
                let staged = Arc::clone(&staged);
                crate::blocking::drive(move || async move {
                    needed(&staged, current.envelope, remote).await
                })
                .await
                .map_err(|e| CommitFailure::Internal(format!("mirror: naming stopped: {e}")))?
                .map_err(|e| CommitFailure::Internal(format!("mirror: {e}")))?
            };
            if let Err(e) = naming
                .named(&needed)
                .instrument(debug_span!("server_mirror_name", ids = needed.len()))
                .await
            {
                warn!(error = %e, "mirror: naming its content failed");
                return Err(CommitFailure::Failed {
                    head: current,
                    message: format!("naming the mirrored content failed: {e}; mirror again"),
                }
                .into());
            }
            Some(Named {
                since: opened,
                ids: needed,
            })
        }
    };

    let key = mirror_key(principal, current, remote)?;
    let accepted =
        commit::accept_validated_forced(entry, &staged, key, current.envelope, remote, named)
            .await?;
    let head = accepted.durable().await?;
    drop(writing);
    Ok(Mirrored {
        head,
        moved: true,
        objects,
        content,
    })
}

/// The remote's envelope, and its objects and content fetched into `staged` from
/// `current`; `None` when the remote is at `current`.
async fn fetch<Fut>(
    staged: &RepositoryStage<Network>,
    open: impl FnOnce() -> Fut,
    current: RepositoryEnvelopeId,
) -> anyhow::Result<Option<(RepositoryEnvelopeId, FetchedObjects, FetchedContent)>>
where
    Fut: Future<Output = anyhow::Result<(WheatSession, RemoteRepoState)>>,
{
    use anyhow::Context as _;

    let (mut session, state) = open().await?;
    if state.envelope == current {
        wheat_remote::close(&mut session).await?;
        return Ok(None);
    }
    // The current head's closure and content are stored, as `since` asks.
    let objects =
        wheat_remote::fetch_objects(&mut session, 1, staged, state.envelope, Some(current))
            .await
            .context("fetch the remote's objects")?;
    let content =
        wheat_remote::fetch_content(&mut session, 2, staged, state.envelope, Some(current))
            .await
            .context("fetch the remote's content")?;
    wheat_remote::close(&mut session).await?;
    Ok(Some((state.envelope, objects, content)))
}

/// `remote`, as arriving on `current`: only what it adds when it extends `current`,
/// whole when it does not.
async fn validate(
    staged: &RepositoryStage<Network>,
    current: RepositoryEnvelopeId,
    remote: RepositoryEnvelopeId,
) -> Result<(), String> {
    let reader = RepositoryReader::new(staged, staged.content());
    if reader
        .validate_arrival(remote, Arrival::Extends(current))
        .await
        .is_ok()
    {
        return Ok(());
    }
    reader
        .validate_arrival(remote, Arrival::Forced)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// What the head moving from `current` to `remote` relies on.
async fn needed(
    staged: &RepositoryStage<Network>,
    current: RepositoryEnvelopeId,
    remote: RepositoryEnvelopeId,
) -> Result<Vec<wheat_core::ContentId>, String> {
    let reader = RepositoryReader::new(staged, staged.content());
    let envelope = async |id| {
        reader
            .envelope(id)
            .await
            .map_err(|e| format!("read an envelope: {}", chain(&e)))
    };
    let before = envelope(current).await?;
    let after = envelope(remote).await?;
    write::needed_over(staged, before.state, after.state).await
}

/// The move's publish key. A mirror has no key of its own to send again: it is the
/// one forced publish from `current` to `remote`, so its key is derived from them,
/// and a mirror that lost its answer after the move gets it when made again before
/// the head moves on.
fn mirror_key(
    principal: &Principal,
    current: RepositoryHead,
    remote: RepositoryEnvelopeId,
) -> Result<PublishKey, MirrorFailure> {
    let request = publish_request_digest(remote, current.generation, current.envelope, true, true);
    let mut key = [0; 16];
    key.copy_from_slice(
        &blake3::Hasher::new_derive_key("wheat-server mirror publish key v1")
            .update(&request)
            .finalize()
            .as_bytes()[..16],
    );
    PublishKey::server_write(principal, key, request).map_err(|e| {
        MirrorFailure::Write(WriteFailure::Invalid(format!(
            "the principal cannot be bound: {e}"
        )))
    })
}
