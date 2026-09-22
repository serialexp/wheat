use std::path::Path;

use anyhow::{Context, Result, ensure};
use wheat_store::RepositoryHead;

use crate::{
    workspace_metadata::{
        MaterializedState, PendingRepositoryOperation, SerializedHead, WorkspaceMetadata,
    },
    workspace_registry::{
        MaterializationRoots, PendingRepositoryCasRoots, RepositoryRoot, StableWorkspaceRoots,
        WorkspacePin, WorkspaceRegistry,
    },
};

fn repository_root(head: SerializedHead) -> RepositoryRoot {
    RepositoryRoot {
        generation: head.generation,
        envelope: head.envelope,
    }
}

fn materialization_roots(state: &MaterializedState) -> MaterializationRoots {
    MaterializationRoots {
        repository_head: repository_root(state.repository_head),
        view: state.view,
        tree: state.tree,
    }
}

fn stable_roots(metadata: &WorkspaceMetadata) -> StableWorkspaceRoots {
    StableWorkspaceRoots {
        basis: metadata.basis,
        observed_label_binding: metadata.observed_label_binding,
        selected_frontier: metadata.selected_frontier,
        selected_change: metadata.selected_change,
        materialized: metadata.materialized.as_ref().map(materialization_roots),
    }
}

fn pending_cas(operation: &PendingRepositoryOperation) -> PendingRepositoryCasRoots {
    PendingRepositoryCasRoots {
        expected_head: repository_root(operation.expected_head),
        proposed_head: repository_root(operation.proposed_head),
        resulting_frontier: operation.resulting_frontier,
    }
}

pub(crate) fn pin_for(
    workspace_root: &Path,
    store_root: &Path,
    metadata: &WorkspaceMetadata,
) -> Result<WorkspacePin> {
    let canonical_workspace = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize Wheat workspace {}", workspace_root.display()))?;
    let canonical_store = store_root
        .canonicalize()
        .with_context(|| format!("canonicalize Wheat store {}", store_root.display()))?;
    ensure!(
        canonical_store == metadata.store.path,
        "workspace metadata store path does not match the opened store"
    );
    let mut pin = WorkspacePin::new(
        metadata.store.store_id,
        metadata.workspace_id,
        metadata.registration_token,
        canonical_workspace,
        stable_roots(metadata),
    );
    pin.pending_repository_cas = metadata
        .pending_repository_operation
        .as_ref()
        .map(pending_cas);
    Ok(pin)
}

pub(crate) fn reconcile(
    workspace_root: &Path,
    store_root: &Path,
    metadata: &WorkspaceMetadata,
) -> Result<()> {
    let registry = WorkspaceRegistry::new(store_root, metadata.store.store_id)?;
    registry.publish(&pin_for(workspace_root, store_root, metadata)?)
}

pub(crate) fn transition_materialization(
    workspace_root: &Path,
    store_root: &Path,
    metadata: &WorkspaceMetadata,
    target_head: RepositoryHead,
    target_view: wheat_core::ViewId,
    target_tree: wheat_core::TreeId,
) -> Result<()> {
    let mut pin = pin_for(workspace_root, store_root, metadata)?;
    pin.pending_materialization = Some(MaterializationRoots {
        repository_head: RepositoryRoot {
            generation: target_head.generation,
            envelope: target_head.envelope,
        },
        view: target_view,
        tree: target_tree,
    });
    WorkspaceRegistry::new(store_root, metadata.store.store_id)?.publish(&pin)
}
