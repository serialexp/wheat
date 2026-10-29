//! Authenticated, bounded workspace-label frontier resolution.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;
use wheat_core::{RepositoryStateRecord, ViewId, ViewLabel};
use wheat_store::{
    FrontierIndex, ImmutableObjectStore, PublicationStore, RepositoryHead, RepositoryReader,
    ViewLineageLimits,
};

use crate::{local::LocalRepository, outcome::CliFailure};

#[derive(Clone, Debug)]
pub struct ResolvedFrontier {
    pub head: RepositoryHead,
    pub basis: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub tree: wheat_core::TreeId,
}

impl ResolvedFrontier {
    pub fn effective_view(&self) -> ViewId {
        self.selected_frontier.unwrap_or(self.basis)
    }
}

#[derive(Debug, Serialize)]
pub struct FrontierList {
    pub schema_version: u32,
    pub label: String,
    pub basis: ViewId,
    pub frontiers: Vec<ViewId>,
}

pub async fn resolve(
    repository: &LocalRepository,
    label: &str,
    requested: Option<ViewId>,
) -> Result<ResolvedFrontier> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    resolve_at_head(repository, &reader, head, envelope, label, requested).await
}

/// The workspace's own pinned basis and selected frontier at the current head, checking
/// only that the head retains them. Unlike [`resolve`], this does not read the label:
/// the label may since have moved or been deleted, and the workspace keeps what it
/// pinned until `refresh`, `integrate`, `pull` or `switch` re-read it.
pub async fn pinned(
    repository: &LocalRepository,
    basis: ViewId,
    selected_frontier: Option<ViewId>,
) -> Result<ResolvedFrontier> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let basis_tree = reader.retained_view(&state, basis).await?.tree;
    let tree = match selected_frontier {
        Some(selected) => reader.retained_view(&state, selected).await?.tree,
        None => basis_tree,
    };
    // As in `resolve_at_head`: never materialize a selection read across two heads.
    anyhow::ensure!(
        repository.heads.read()? == head,
        "repository head changed while reading the workspace's pinned views"
    );
    Ok(ResolvedFrontier {
        head,
        basis,
        selected_frontier,
        tree,
    })
}

/// Resolve directly from the exact head returned by a just-completed atomic import.
///
/// Atomic import completely validates the envelope immediately before publishing it,
/// and the installed store is immutable between that return and this call. Reopening
/// authenticates the packed catalog; checking the publication pointer preserves the
/// snapshot boundary without repeating a complete retained-history traversal.
pub async fn resolve_freshly_validated_import(
    repository: &LocalRepository,
    validated_head: RepositoryHead,
    label: &str,
) -> Result<ResolvedFrontier> {
    anyhow::ensure!(
        repository.heads.read()? == validated_head,
        "repository head changed after validated import"
    );
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(validated_head.envelope).await?;
    resolve_at_head(repository, &reader, validated_head, envelope, label, None).await
}

async fn resolve_at_head<O: wheat_store::ImmutableObjectStore>(
    repository: &LocalRepository,
    reader: &RepositoryReader<'_, O>,
    head: RepositoryHead,
    envelope: wheat_core::RepositoryEnvelopeRecord,
    label: &str,
    requested: Option<ViewId>,
) -> Result<ResolvedFrontier> {
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
    let frontiers = LabelFrontiers {
        index: &repository.frontiers,
        labels: &labels.labels,
    }
    .of(&repository.objects, &state, basis)
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
    let tree = reader
        .retained_view(&state, selected_frontier.unwrap_or(basis))
        .await?
        .tree;
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

/// Where a command finds a view's descendant frontiers: the store's frontier index,
/// which keeps them for the views `labels` point at.
#[derive(Clone, Copy)]
pub struct LabelFrontiers<'a> {
    pub index: &'a FrontierIndex,
    pub labels: &'a BTreeMap<String, ViewLabel>,
}

impl LabelFrontiers<'_> {
    /// `view`'s descendant frontiers in `state`.
    pub async fn of<O: ImmutableObjectStore>(
        &self,
        objects: &O,
        state: &RepositoryStateRecord,
        view: ViewId,
    ) -> Result<Vec<ViewId>> {
        Ok(self
            .index
            .frontiers(
                objects,
                state.view_lineage,
                self.labels.values().map(|label| label.view),
                view,
                ViewLineageLimits::default(),
            )
            .await?)
    }
}

#[tracing::instrument(name = "frontier_list", level = "debug", skip_all)]
pub async fn list(repository: &LocalRepository, label: &str) -> Result<FrontierList> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
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
    reader.retained_view(&state, basis).await?;
    let frontiers = LabelFrontiers {
        index: &repository.frontiers,
        labels: &labels.labels,
    }
    .of(&repository.objects, &state, basis)
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

/// The frontiers of `workspace`'s active label.
pub fn read(
    workspace: &std::path::Path,
    explicit_store: Option<&std::path::Path>,
) -> Result<FrontierList> {
    let context = crate::discovery::resolve_context(workspace, explicit_store)?;
    let repository = crate::local::open(&context.store_root)?;
    let _operation =
        crate::rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &repository)?;
    crate::local::runtime()?.block_on(list(&repository, &context.workspace.active_label))
}
