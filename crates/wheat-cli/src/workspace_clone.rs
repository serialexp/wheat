use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use wheat_store::{PublicationStore, RepositoryReader};

use crate::{
    command_output::Success,
    discovery::{canonical_store_path, discover_workspace},
    dot_git::DotGitGate,
    frontier_service,
    local::{open, runtime},
    materialize_resolved,
    workspace_metadata::{StoreMetadata, WorkspaceMetadata},
    workspace_registry_service,
};

pub(crate) fn clone_local(
    source: &Path,
    workspace_root: &Path,
    dot_git: DotGitGate,
) -> Result<Success> {
    super::ensure_fresh_workspace(workspace_root)?;
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
    let head = repository.heads.read()?;
    let rt = runtime()?;
    let (label, resolved) = rt.block_on(async {
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.envelope(head.envelope).await?;
        let labels = reader.view_label_manifest(envelope.labels).await?;
        let label = labels
            .labels
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "main".to_owned());
        let resolved = match frontier_service::resolve(&repository, &label, None).await {
            Ok(resolved) => resolved,
            Err(_) => {
                let basis = labels
                    .labels
                    .get(&label)
                    .context("selected label disappeared")?
                    .view;
                frontier_service::resolve(&repository, &label, Some(basis)).await?
            }
        };
        Ok::<_, anyhow::Error>((label, resolved))
    })?;
    drop(repository);

    super::initialize_workspace_directory(workspace_root)?;
    let metadata = WorkspaceMetadata::new(
        canonical_store_path(&store_root)?,
        store.store_id,
        label.clone(),
        resolved.basis,
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

pub(crate) fn local_source(value: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    path.exists().then_some(path)
}
