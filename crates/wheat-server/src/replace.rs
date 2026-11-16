//! Replacing a repository's head with an envelope staged over its network, as a
//! forced publish naming the current head would: what [`crate::mirror`] and
//! [`crate::restore`] share around their staging.
//!
//! [`Replacement::begin`] takes a slot among the repository's writers before it reads
//! the head, and the slot is held until the head has moved, so a quiesce (an export,
//! a collection) waits for the whole staging: what the stage stored early is in the
//! pool before a head names it. [`Replacement::finish`] validates what was staged,
//! names the content the move relies on first when the network is told that
//! ([`crate::naming`]), and joins the repository's group commit with the validation
//! already done, made durable in its batch. No publish hooks run: the embedder tells
//! its own listeners.

use std::sync::Arc;

use tokio::time::Instant;
use tracing::{Instrument as _, debug_span, warn};
use wheat_core::{RepositoryEnvelopeId, publish_request_digest};
use wheat_store::{Arrival, RepositoryHead, RepositoryReader, RepositoryStage};

use crate::{
    access::Principal,
    commit::{self, CommitFailure},
    error_text::chain,
    naming::Named,
    network::Network,
    quiesce::Writing,
    receipt::PublishKey,
    registry::RepositoryEntry,
    write::{self, WriteFailure},
};

/// How [`Replacement::finish`] validates the staged envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Validation {
    /// Only what it adds when it extends the current head, whole otherwise.
    ExtendingWhenItCan,
    /// Whole, as [`Arrival::Forced`].
    Whole,
}

/// A move of a repository's head under way: the head it moves from, and the stage
/// what it moves to is staged in.
pub(crate) struct Replacement {
    pub(crate) staged: Arc<RepositoryStage<Network>>,
    /// The head when the move began, which it moves from.
    pub(crate) current: RepositoryHead,
    /// Before anything was staged: each stamp of the staged content is newer.
    opened: Instant,
    _writing: Writing,
}

impl Replacement {
    /// Takes a writer slot and reads the head.
    pub(crate) async fn begin(entry: &Arc<RepositoryEntry>) -> Result<Self, WriteFailure> {
        let writing = write::begin(entry)?;
        let opened = Instant::now();
        let current = entry.heads.head().await.map_err(|e| {
            WriteFailure::Commit(CommitFailure::Internal(format!(
                "read the head: {}",
                chain(&e)
            )))
        })?;
        let staged = Arc::new(RepositoryStage::new(
            entry.network.clone(),
            write::STAGE_OBJECTS,
            write::STAGE_BYTES,
        ));
        Ok(Self {
            staged,
            current,
            opened,
            _writing: writing,
        })
    }

    /// Moves the head to `envelope`, staged in [`Self::staged`], validated as
    /// `validation` says, for `principal`. `key_context` derives the move's publish
    /// key (see [`key`]); each kind of move has its own. Returns the new head.
    pub(crate) async fn finish(
        self,
        entry: &Arc<RepositoryEntry>,
        principal: &Principal,
        envelope: RepositoryEnvelopeId,
        validation: Validation,
        key_context: &'static str,
    ) -> Result<RepositoryHead, WriteFailure> {
        let current = self.current;
        {
            let staged = Arc::clone(&self.staged);
            crate::blocking::drive(move || async move {
                validate(&staged, current.envelope, envelope, validation).await
            })
            .await
            .map_err(|e| internal(format!("validation stopped: {e}")))?
            .map_err(|message| {
                WriteFailure::Commit(CommitFailure::Rejected {
                    head: current,
                    message: format!("the new head failed validation: {message}"),
                })
            })?;
        }

        let named = match &entry.network.naming {
            None => None,
            Some(naming) => {
                let needed = {
                    let staged = Arc::clone(&self.staged);
                    crate::blocking::drive(move || async move {
                        needed(&staged, current.envelope, envelope).await
                    })
                    .await
                    .map_err(|e| internal(format!("naming stopped: {e}")))?
                    .map_err(internal)?
                };
                if let Err(e) = naming
                    .named(&needed)
                    .instrument(debug_span!("server_replace_name", ids = needed.len()))
                    .await
                {
                    warn!(error = %e, "naming the content of a replaced head failed");
                    return Err(WriteFailure::Commit(CommitFailure::Failed {
                        head: current,
                        message: format!("naming the new head's content failed: {e}; try again"),
                    }));
                }
                Some(Named {
                    since: self.opened,
                    ids: needed,
                })
            }
        };

        let key = key(principal, current, envelope, key_context)?;
        let accepted = commit::accept_validated_forced(
            entry,
            &self.staged,
            key,
            current.envelope,
            envelope,
            named,
        )
        .await
        .map_err(WriteFailure::Commit)?;
        let head = accepted.durable().await.map_err(WriteFailure::Commit)?;
        drop(self);
        Ok(head)
    }
}

fn internal(message: String) -> WriteFailure {
    WriteFailure::Commit(CommitFailure::Internal(message))
}

/// `new`, as arriving on `current`.
async fn validate(
    staged: &RepositoryStage<Network>,
    current: RepositoryEnvelopeId,
    new: RepositoryEnvelopeId,
    validation: Validation,
) -> Result<(), String> {
    let reader = RepositoryReader::new(staged, staged.content());
    if validation == Validation::ExtendingWhenItCan
        && reader
            .validate_arrival(new, Arrival::Extends(current))
            .await
            .is_ok()
    {
        return Ok(());
    }
    reader
        .validate_arrival(new, Arrival::Forced)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// What the head moving from `current` to `new` relies on.
async fn needed(
    staged: &RepositoryStage<Network>,
    current: RepositoryEnvelopeId,
    new: RepositoryEnvelopeId,
) -> Result<Vec<wheat_core::ContentId>, String> {
    let reader = RepositoryReader::new(staged, staged.content());
    let envelope = async |id| {
        reader
            .envelope(id)
            .await
            .map_err(|e| format!("read an envelope: {}", chain(&e)))
    };
    let before = envelope(current).await?;
    let after = envelope(new).await?;
    write::needed_over(staged, before.state, after.state).await
}

/// The move's publish key. A replacement has no key of its own to send again: it is
/// the one forced publish from `current` to `new`, so its key is derived from them
/// (under `context`), and a move that lost its answer after the head moved gets it
/// when made again before the head moves on.
fn key(
    principal: &Principal,
    current: RepositoryHead,
    new: RepositoryEnvelopeId,
    context: &'static str,
) -> Result<PublishKey, WriteFailure> {
    let request = publish_request_digest(new, current.generation, current.envelope, true, true);
    let mut key = [0; 16];
    key.copy_from_slice(
        &blake3::Hasher::new_derive_key(context)
            .update(&request)
            .finalize()
            .as_bytes()[..16],
    );
    PublishKey::server_write(principal, key, request)
        .map_err(|e| WriteFailure::Invalid(format!("the principal cannot be bound: {e}")))
}
