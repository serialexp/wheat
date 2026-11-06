//! Writes an embedder starts, with no client session: Gothab's ref writes, for
//! example. A write takes its turn in the repository's group commit as a publish
//! does, so it is serialized with live publishes and made durable in their batches,
//! and its key makes it idempotent as a publish key does (see [`crate::receipt`]).
//! No publish hooks run: the embedder tells its own listeners.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

pub use wheat_core::ExpectedLabel;
use wheat_core::{ViewId, ViewLabel, ViewLabelManifestRecord, server_write_request_digest};
use wheat_store::{RepositoryHead, RepositoryStage};

use crate::{
    access::Principal,
    commit::{self, CommitFailure, LabelRefusal},
    receipt::PublishKey,
    registry::RepositoryEntry,
};

/// The most objects, and bytes, a server write stages: its label manifest and
/// envelope.
const STAGED_OBJECTS: usize = 16;
const STAGED_BYTES: usize = 16 * 1024 * 1024;

/// One label a write changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelChange {
    pub name: String,
    /// What the label must be on the head the write joins onto.
    pub expected: ExpectedLabel,
    /// What the label becomes; `None` deletes it. A [`ViewLabel`], so a tag keeps its
    /// annotation.
    pub new: Option<ViewLabel>,
}

/// A write of a repository's labels.
#[derive(Clone, Debug)]
pub struct ServerWrite {
    /// Who the write is for: receipts are kept per principal and key.
    pub principal: Principal,
    /// The embedder's idempotency key. The same key with the same changes gets the
    /// first answer; with other changes it is refused.
    pub key: [u8; 16],
    pub labels: Vec<LabelChange>,
    /// As for a publish: otherwise every label moved must move forward.
    pub force_labels: bool,
}

/// Why a write was not made.
#[derive(Clone, Debug)]
pub enum WriteFailure {
    /// The changes are not a write: none, a label named twice, one expected absent
    /// and deleted, or a label that is not valid.
    Invalid(String),
    /// Labels are not where the write expects them: each with its view on the head
    /// the write would have joined onto (none when absent). `head` is the durable
    /// head.
    LabelMoved {
        head: RepositoryHead,
        labels: Vec<(String, Option<ViewId>)>,
    },
    /// The network refuses writes for the moment (it is being exported or moved);
    /// try again after `retry_after` when it says.
    Quiesced { retry_after: Option<Duration> },
    /// As [`CommitFailure`]: refused, or the store failed.
    Commit(CommitFailure),
}

/// Makes `write` to the repository `entry` and answers the head it was published in.
#[tracing::instrument(name = "server_write", level = "debug", skip_all)]
pub async fn write(
    entry: &Arc<RepositoryEntry>,
    write: ServerWrite,
) -> Result<RepositoryHead, WriteFailure> {
    let request = request_digest(&write)?;
    let key = PublishKey::server_write(&write.principal, write.key, request)
        .map_err(|e| WriteFailure::Invalid(format!("the principal cannot be bound: {e}")))?;
    // Held until the write is answered, so a quiesce waits for it.
    let Some(_writing) = entry.network.writers.begin() else {
        return Err(WriteFailure::Quiesced {
            retry_after: entry.network.writers.retry_after(),
        });
    };
    let staged = Arc::new(RepositoryStage::new(
        entry.network.clone(),
        STAGED_OBJECTS,
        STAGED_BYTES,
    ));
    let accepted = commit::accept_labels(
        entry,
        &staged,
        key,
        Arc::new(write.labels),
        write.force_labels,
    )
    .await
    .map_err(|refusal| match refusal {
        LabelRefusal::Moved { head, labels } => WriteFailure::LabelMoved { head, labels },
        LabelRefusal::Failed(failure) => WriteFailure::Commit(failure),
    })?;
    accepted.durable().await.map_err(WriteFailure::Commit)
}

/// What `write` asks for, digested, once its changes are checked to be a write.
fn request_digest(write: &ServerWrite) -> Result<[u8; 16], WriteFailure> {
    if write.labels.is_empty() {
        return Err(WriteFailure::Invalid(
            "a write changes at least one label".into(),
        ));
    }
    let mut names = BTreeSet::new();
    for change in &write.labels {
        if !names.insert(change.name.as_str()) {
            return Err(WriteFailure::Invalid(format!(
                "label {:?} is changed twice",
                change.name
            )));
        }
        if change.expected == ExpectedLabel::Absent && change.new.is_none() {
            return Err(WriteFailure::Invalid(format!(
                "label {:?} neither exists before nor after the write",
                change.name
            )));
        }
        ViewLabelManifestRecord::validate_entry(&change.name, change.new.as_ref())
            .map_err(|e| WriteFailure::Invalid(format!("the labels are not valid: {e}")))?;
    }
    Ok(server_write_request_digest(
        write
            .labels
            .iter()
            .map(|change| (change.name.as_str(), change.expected, change.new.as_ref())),
        write.force_labels,
    ))
}
