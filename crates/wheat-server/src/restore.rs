//! Restoring a served repository to a snapshot. The snapshot is authoritative:
//! [`restore_from`] makes the repository's head the snapshot's envelope, labels and
//! all, replacing whatever the repository held, as a forced publish naming the
//! current head would. The head's generation moves on from the current one; the
//! generation the snapshot recorded is not restored.
//!
//! The snapshot's manifest is authenticated against the id its source claims, its
//! objects and content are staged over the network's store, checked against the
//! manifest's inventory, and the envelope is validated whole, as a forced fetch is.
//! The move then joins the repository's group commit as [`crate::mirror`]'s does:
//! the content it relies on named first when the network is told that
//! ([`crate::naming`]), and made durable in its batch. No publish hooks run.
//!
//! The restore holds a slot among the repository's writers from before it reads the
//! head until the head has moved, so a quiesce (an export, a collection) waits for
//! the whole staging. Staging reads the snapshot's whole closure, then walks it
//! twice (the inventory check and the validation): restoring costs in proportion to
//! the snapshot, however little it differs from the head.

use std::sync::Arc;

use wheat_store::{
    RepositoryHead, SnapshotError, SnapshotLimits, SnapshotSource, read_snapshot_manifest,
    stage_snapshot_from,
};

use crate::{
    access::Principal,
    commit::CommitFailure,
    registry::RepositoryEntry,
    replace::{Replacement, Validation},
    write::WriteFailure,
};

/// What [`restore_from`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Restored {
    /// The repository's head, at the snapshot's envelope.
    pub head: RepositoryHead,
    /// Whether the head moved: false when it was at the snapshot's envelope already,
    /// and nothing was staged.
    pub moved: bool,
}

/// Why [`restore_from`] did not move the head. The head is as it was.
#[derive(Debug)]
pub enum RestoreFailure {
    /// The snapshot could not be read, is not the one its source claims, breaks a
    /// limit, or its payload is not its manifest's inventory.
    Snapshot(SnapshotError),
    /// The head could not be moved: as [`crate::mirror::MirrorFailure::Write`].
    /// [`CommitFailure::Rejected`] when the snapshot's envelope is not a valid
    /// repository.
    Write(WriteFailure),
}

impl From<CommitFailure> for RestoreFailure {
    fn from(failure: CommitFailure) -> Self {
        Self::Write(WriteFailure::Commit(failure))
    }
}

/// Makes the head of the repository `entry` the envelope of the snapshot `source`
/// holds, read within `limits`. `principal` is who the move is recorded for in the
/// head's receipts.
///
/// The manifest is authenticated against the id `source` claims for it
/// ([`SnapshotSource::read_manifest`]), so the claim should come from the caller's own
/// record of the snapshot, not from the storage that holds its bytes. `source` is
/// read on a thread for blocking work.
#[tracing::instrument(name = "server_restore", level = "debug", skip_all)]
pub async fn restore_from<S: SnapshotSource + 'static>(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    source: Arc<S>,
    limits: SnapshotLimits,
) -> Result<Restored, RestoreFailure> {
    let replacement = Replacement::begin(entry)
        .await
        .map_err(RestoreFailure::Write)?;
    let current = replacement.current;
    let staged = {
        let staged = Arc::clone(&replacement.staged);
        crate::blocking::drive(move || async move {
            let manifest = read_snapshot_manifest(source.as_ref(), limits).await?;
            if manifest.envelope == current.envelope {
                return Ok(None);
            }
            stage_snapshot_from(source.as_ref(), &manifest, &staged, limits).await?;
            Ok(Some(manifest.envelope))
        })
        .await
        .map_err(|e| CommitFailure::Internal(format!("restore: staging stopped: {e}")))?
        .map_err(RestoreFailure::Snapshot)?
    };
    let Some(envelope) = staged else {
        return Ok(Restored {
            head: current,
            moved: false,
        });
    };
    let head = replacement
        .finish(
            entry,
            principal,
            envelope,
            Validation::Whole,
            "wheat-server restore publish key v1",
        )
        .await
        .map_err(RestoreFailure::Write)?;
    Ok(Restored { head, moved: true })
}
