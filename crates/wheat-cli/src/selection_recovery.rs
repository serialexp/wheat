use anyhow::{Context, Result};
use wheat_store::{ChangeMapStore, PublicationStore, RepositoryReader, RevisionMapStore};

use crate::{local::LocalRepository, workspace_metadata::WorkspaceMetadata};

pub(crate) fn recover(
    root: &std::path::Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    let Some(pending) = metadata.pending_selection.clone() else {
        return Ok(());
    };
    let head = repository.heads.read()?;
    let runtime = crate::local::runtime()?;
    let present = runtime.block_on(async {
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.envelope(head.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let working = reader.view(state.working_view).await?;
        let revision = RevisionMapStore::new(&repository.objects)
            .get(working.revisions, pending.change)
            .await?;
        let object = ChangeMapStore::new(&repository.objects)
            .get(state.changes, pending.change)
            .await?;
        Ok::<_, anyhow::Error>(revision.is_some() && object.is_some())
    })?;
    if present {
        metadata.selected_change = Some(pending.change);
    } else {
        let expected: wheat_store::RepositoryHead = pending.expected_head.into();
        anyhow::ensure!(
            head == expected,
            "pending change selection is absent after unrelated repository advancement"
        );
    }
    metadata.pending_selection = None;
    metadata
        .replace_prepared(root)
        .context("prepare recovered workspace selection")?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))
}
