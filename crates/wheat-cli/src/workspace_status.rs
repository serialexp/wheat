use serde::Serialize;
use wheat_core::{ViewId, diff_model::DiffDescriptor};

use crate::workspace_scan::WorkspaceScan;

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceStatus {
    pub schema_version: u32,
    pub materialized_view: ViewId,
    pub materialized_head_generation: u64,
    pub current_head_generation: u64,
    pub authority_advanced: bool,
    pub active_label: String,
    pub basis: ViewId,
    pub observed_label_binding: ViewId,
    pub current_label_binding: Option<ViewId>,
    pub label_binding_drifted: bool,
    pub selected_frontier: Option<ViewId>,
    pub effective_view: ViewId,
    pub changes: Vec<TrackedChange>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TrackedChange {
    pub path: String,
    pub content_changed: bool,
    pub kind_changed: bool,
    pub deleted: bool,
    pub baseline: DiffDescriptor,
    pub workspace: Option<DiffDescriptor>,
}

impl From<WorkspaceScan> for WorkspaceStatus {
    fn from(scan: WorkspaceScan) -> Self {
        Self {
            schema_version: 1,
            materialized_view: scan.materialized_view,
            materialized_head_generation: scan.materialized_head_generation,
            current_head_generation: scan.current_head_generation,
            authority_advanced: scan.authority_advanced,
            active_label: scan.active_label,
            basis: scan.basis,
            observed_label_binding: scan.observed_label_binding,
            current_label_binding: scan.current_label_binding,
            label_binding_drifted: scan.label_binding_drifted,
            selected_frontier: scan.selected_frontier,
            effective_view: scan.effective_view,
            changes: scan
                .changes
                .into_iter()
                .map(|change| TrackedChange {
                    path: change.path,
                    content_changed: change.content_changed,
                    kind_changed: change.kind_changed,
                    deleted: change.workspace.is_none(),
                    baseline: change.baseline,
                    workspace: change.workspace,
                })
                .collect(),
        }
    }
}
