//! The changes a workspace's working view selects, each with its current revision,
//! for browsing history; and one change's earlier revisions, read on demand.

use std::path::Path;

use anyhow::Result;
use wheat_core::{Actor, ChangeId, RevisionId, Timestamp};
use wheat_store::{ChangeMapStore, PublicationStore, RepositoryReader, RevisionMapStore};

use crate::{
    discovery::resolve_context,
    local::{open, runtime},
    outcome::CliFailure,
    rewrite_transaction::StoreOperationGuard,
    workspace_metadata::WorkspaceMetadata,
};

/// The most changes one history read returns, as `wheat log` allows.
const MAX_HISTORY_CHANGES: usize = 250_000;

/// One change in the working view, at its current revision.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub change: ChangeId,
    pub title: String,
    pub creator: Actor,
    pub revision: RevisionId,
    /// The current revision's message and author.
    pub message: String,
    pub author: Actor,
    /// The current revision's generation: 0 for a change's first revision, one more
    /// for each revision along its predecessors (and combined revisions).
    pub generation: u64,
    pub dependencies: Vec<ChangeId>,
    pub committed_at: Timestamp,
}

/// One revision of a change.
#[derive(Clone, Debug)]
pub struct RevisionEntry {
    pub revision: RevisionId,
    pub message: String,
    pub author: Actor,
    pub committed_at: Timestamp,
}

/// Every change the workspace's working view selects, newest commit first. Reads
/// three objects per change: the cost of `wheat log`.
#[tracing::instrument(name = "history", level = "debug", skip_all)]
pub fn read(workspace: &Path, explicit_store: Option<&Path>) -> Result<Vec<HistoryEntry>> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation = StoreOperationGuard::acquire(&context.store_root, &local)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    let effective_view = context.workspace.effective_view();
    let mut entries = runtime()?.block_on(async {
        let head = local.heads.read()?;
        let reader = RepositoryReader::new(&local.objects, &local.content);
        let envelope = reader.envelope(head.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let working = reader.view(effective_view).await?;
        if working.id() != effective_view {
            return Err(CliFailure::corrupt("effective view identity mismatch"));
        }
        let revisions = RevisionMapStore::new(&local.objects);
        let changes = ChangeMapStore::new(&local.objects);
        let mut cursor = revisions.cursor(working.revisions).await?;
        let mut entries = Vec::new();
        while let Some((change_id, revision_id)) = cursor.next().await? {
            if entries.len() == MAX_HISTORY_CHANGES {
                return Err(CliFailure::limit(
                    "current revision count exceeds the history limit",
                ));
            }
            let revision = reader.revision(revision_id).await?;
            if revision.id() != revision_id || revision.change_id != change_id {
                return Err(CliFailure::corrupt("revision identity mismatch"));
            }
            let change_object = changes
                .get(state.changes, change_id)
                .await?
                .ok_or_else(|| {
                    CliFailure::corrupt("revision refers to an unindexed logical change")
                })?;
            let change = reader.change(change_object).await?;
            if change.id != change_id {
                return Err(CliFailure::corrupt("logical change identity mismatch"));
            }
            entries.push(HistoryEntry {
                change: change_id,
                title: change.metadata.title,
                creator: change.metadata.creator,
                revision: revision_id,
                message: revision.metadata.message.clone(),
                author: revision.metadata.author.clone(),
                generation: revision.generation,
                dependencies: revision.dependencies.iter().copied().collect(),
                committed_at: revision.metadata.committed_at,
            });
        }
        Ok(entries)
    })?;
    entries.sort_by(|left, right| {
        right
            .committed_at
            .seconds
            .cmp(&left.committed_at.seconds)
            .then_with(|| right.change.cmp(&left.change))
    });
    Ok(entries)
}

/// `revision` and the revisions before it along its predecessors, newest first, at
/// most `limit` of them.
pub fn revisions(
    workspace: &Path,
    explicit_store: Option<&Path>,
    revision: RevisionId,
    limit: usize,
) -> Result<Vec<RevisionEntry>> {
    let context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation = StoreOperationGuard::acquire(&context.store_root, &local)?;
    runtime()?.block_on(async {
        let reader = RepositoryReader::new(&local.objects, &local.content);
        let mut chain = Vec::new();
        let mut next = Some(revision);
        while let Some(id) = next {
            if chain.len() == limit {
                break;
            }
            let record = reader.revision(id).await?;
            if record.id() != id {
                return Err(CliFailure::corrupt("revision identity mismatch"));
            }
            next = record.predecessor;
            chain.push(RevisionEntry {
                revision: id,
                message: record.metadata.message.clone(),
                author: record.metadata.author.clone(),
                committed_at: record.metadata.committed_at,
            });
        }
        Ok(chain)
    })
}
