use anyhow::{Context, Result};
use wheat_store::PublicationStore;

use crate::{local::LocalRepository, workspace_metadata::WorkspaceMetadata};

/// Complete or discard a workspace publication intent after an interrupted repository CAS.
#[tracing::instrument(name = "recover_selection", level = "debug", skip_all)]
pub(crate) fn recover(
    root: &std::path::Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    let Some(pending) = metadata.pending_repository_operation.clone() else {
        return Ok(());
    };
    let head = repository.heads.read()?;
    let expected: wheat_store::RepositoryHead = pending.expected_head.into();
    let proposed: wheat_store::RepositoryHead = pending.proposed_head.into();
    if head == proposed {
        metadata.selected_frontier =
            (pending.resulting_frontier != metadata.basis).then_some(pending.resulting_frontier);
        metadata.selected_change = pending.selected_change;
    } else {
        anyhow::ensure!(
            head == expected,
            "pending workspace operation is unresolved after unrelated repository advancement"
        );
    }
    metadata.pending_repository_operation = None;
    metadata
        .replace_prepared(root)
        .context("prepare recovered workspace operation")?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)
}
