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
    pub working_view: ViewId,
    pub published_view: ViewId,
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
            working_view: scan.working_view,
            published_view: scan.published_view,
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
