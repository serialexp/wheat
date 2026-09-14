//! Authenticated, bounded workspace-label frontier resolution.

use anyhow::Result;
use serde::Serialize;
use wheat_core::ViewId;
use wheat_store::{
    PublicationStore, RepositoryHead, RepositoryReader, ViewLineageLimits, ViewLineageMapStore,
};

use crate::{command_output::CliFailure, local::LocalRepository};

#[derive(Clone, Debug)]
pub(crate) struct ResolvedFrontier {
    pub head: RepositoryHead,
    pub basis: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub tree: wheat_core::TreeId,
}

impl ResolvedFrontier {
    pub(crate) fn effective_view(&self) -> ViewId {
        self.selected_frontier.unwrap_or(self.basis)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct FrontierList {
    pub schema_version: u32,
    pub label: String,
    pub basis: ViewId,
    pub frontiers: Vec<ViewId>,
}

pub(crate) async fn resolve(
    repository: &LocalRepository,
    label: &str,
    requested: Option<ViewId>,
) -> Result<ResolvedFrontier> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.validate_envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let labels = reader.view_label_manifest(envelope.labels).await?;
    if label.is_empty() {
        return Err(CliFailure::invalid("label name must not be empty"));
    }
    let basis = labels
        .labels
        .get(label)
        .map(|binding| binding.view)
        .ok_or_else(|| CliFailure::invalid(format!("unknown Wheat label {label}")))?;
    let frontiers = ViewLineageMapStore::new(&repository.objects)
        .descendant_frontiers(state.view_lineage, basis, ViewLineageLimits::default())
        .await?;
    let selected_frontier = match requested {
        Some(requested) if requested == basis => None,
        Some(requested) => {
            if !frontiers.contains(&requested) {
                return Err(CliFailure::invalid(format!(
                    "view {requested} is not a descendant frontier of label {label}"
                )));
            }
            Some(requested)
        }
        None => match frontiers.as_slice() {
            [] => None,
            [only] => Some(*only),
            _ => {
                return Err(CliFailure::invalid(format!(
                    "label {label} has {} descendant frontiers; select one with --frontier",
                    frontiers.len()
                )));
            }
        },
    };
    let tree = reader.view(selected_frontier.unwrap_or(basis)).await?.tree;
    // Re-read the publication pointer after all authenticated traversal so callers never
    // materialize a selection assembled across two repository snapshots.
    anyhow::ensure!(
        repository.heads.read()? == head,
        "repository head changed while resolving label frontiers"
    );
    Ok(ResolvedFrontier {
        head,
        basis,
        selected_frontier,
        tree,
    })
}

pub(crate) async fn list(repository: &LocalRepository, label: &str) -> Result<FrontierList> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.validate_envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let labels = reader.view_label_manifest(envelope.labels).await?;
    if label.is_empty() {
        return Err(CliFailure::invalid("label name must not be empty"));
    }
    let basis = labels
        .labels
        .get(label)
        .map(|value| value.view)
        .ok_or_else(|| CliFailure::invalid(format!("unknown Wheat label {label}")))?;
    let frontiers = ViewLineageMapStore::new(&repository.objects)
        .descendant_frontiers(state.view_lineage, basis, ViewLineageLimits::default())
        .await?;
    anyhow::ensure!(
        repository.heads.read()? == head,
        "repository head changed while listing label frontiers"
    );
    Ok(FrontierList {
        schema_version: 1,
        label: label.to_owned(),
        basis,
        frontiers,
    })
}
