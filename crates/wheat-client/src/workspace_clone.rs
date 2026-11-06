use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use wheat_core::ViewId;
use wheat_store::{PublicationStore, RepositoryReader};

use crate::{
    discovery::{canonical_store_path, discover_workspace},
    dot_git::DotGitGate,
    frontier_service::{self, ResolvedFrontier},
    hidden_labels::{self, LabelFilter},
    local::{LocalRepository, open, runtime},
    materialization::materialize_resolved,
    outcome::Success,
    workspace_lifecycle::{ensure_fresh_workspace, initialize_workspace_directory},
    workspace_metadata::{StoreMetadata, WorkspaceMetadata},
    workspace_registry_service,
};

/// What a new workspace shows.
#[derive(Clone, Copy, Debug)]
pub enum CloneStart {
    /// The default label (`main`, or the first one shown), at its view when it has
    /// several descendant frontiers.
    DefaultLabel,
    /// Exactly this view, following no label.
    View(ViewId),
}

/// The label a new workspace on `repository` follows, if any, and what it shows.
pub async fn resolve_start(
    repository: &LocalRepository,
    filter: &LabelFilter,
    start: CloneStart,
) -> Result<(Option<String>, ResolvedFrontier)> {
    if let CloneStart::View(view) = start {
        return Ok((
            None,
            frontier_service::resolve_view(repository, view).await?,
        ));
    }
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let labels = reader.view_label_manifest(envelope.labels).await?;
    let label = filter.default_label(labels.labels.keys());
    let resolved = match frontier_service::resolve(repository, &label, None).await {
        Ok(resolved) => resolved,
        // Several descendant frontiers: start at the label's own view.
        Err(_) => {
            let basis = labels
                .labels
                .get(&label)
                .context("selected label disappeared")?
                .view;
            frontier_service::resolve(repository, &label, Some(basis)).await?
        }
    };
    Ok((Some(label), resolved))
}

/// A new workspace on the store at or of `source`, showing `start`. `shown` adds
/// hidden label patterns to the store's remote binding first ([`crate::hidden_labels`]).
pub fn clone_local(
    source: &Path,
    workspace_root: &Path,
    shown: &[String],
    start: CloneStart,
    dot_git: DotGitGate,
) -> Result<Success> {
    ensure_fresh_workspace(workspace_root)?;
    let store_root = match discover_workspace(source) {
        Ok(context) => context.store_root,
        Err(_) => {
            let store = canonical_store_path(source).with_context(|| {
                format!(
                    "source is not a Wheat workspace or store: {}",
                    source.display()
                )
            })?;
            StoreMetadata::read(&store)?;
            store
        }
    };
    let store = StoreMetadata::read(&store_root)?;
    let repository = open(&store_root)?;
    let _operation =
        crate::rewrite_transaction::StoreOperationGuard::acquire(&store_root, &repository)?;
    if !shown.is_empty() {
        hidden_labels::update_shown(&store_root, shown, &[])?;
    }
    let filter = LabelFilter::of_store(&store_root, false)?;
    let (label, resolved) = runtime()?.block_on(resolve_start(&repository, &filter, start))?;
    drop(repository);

    initialize_workspace_directory(workspace_root)?;
    let metadata = WorkspaceMetadata::new(
        canonical_store_path(&store_root)?,
        store.store_id,
        label.clone(),
        resolved.basis,
        resolved.selected_frontier,
    );
    workspace_registry_service::reconcile(workspace_root, &store_root, &metadata)?;
    metadata.initialize(workspace_root)?;
    let (view, root) =
        materialize_resolved(workspace_root, Some(&store_root), label, resolved, dot_git)?;
    ensure!(
        root == workspace_root.canonicalize()?,
        "materialized a different workspace root"
    );
    Success::new(
        "clone",
        json!({
            "source": source,
            "store": store_root,
            "workspace_id": metadata.workspace_id.to_string(),
            "view": view.to_string(),
            "shared_store": true,
        }),
        format!(
            "created workspace {} at view {view}\nreused store {}\n",
            workspace_root.display(),
            store_root.display()
        ),
    )
}

pub fn local_source(value: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    path.exists().then_some(path)
}
