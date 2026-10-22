//! One-pass comparison of the workspace with its pinned materialized tree.

use std::path::Path;

use anyhow::{Context, Result};
use wheat_core::{ContentId, FileKind, ViewId, diff_model::DiffDescriptor, text_capture};
use wheat_store::{
    ContentStoreError, MerkleTreeStore, PublicationStore, RepositoryHead, RepositoryReader,
    TreePathFilter, TreeWalkLimits,
};

use crate::{
    local::LocalRepository,
    workspace_files::{absent_where_shape_changed, analyze_workspace_file},
    workspace_metadata::WorkspaceMetadata,
};

const MAX_WORKSPACE_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BASELINE_INSPECTION_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_READ_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct WorkspaceScan {
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
    pub changes: Vec<ScannedChange>,
    /// Complete set of tracked paths from the materialized tree.
    pub tracked_paths: std::collections::BTreeSet<String>,
}

#[derive(Debug)]
pub struct ScannedChange {
    pub path: String,
    pub content_changed: bool,
    pub kind_changed: bool,
    pub baseline: DiffDescriptor,
    pub workspace: Option<DiffDescriptor>,
    /// Canonical text is retained only when the caller's inline bound permits it.
    pub baseline_text: Option<Vec<u8>>,
    pub workspace_text: Option<Vec<u8>>,
}

pub fn scan(
    workspace_root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
    max_inline_text_bytes: usize,
    filter: &TreePathFilter,
) -> Result<WorkspaceScan> {
    let _lock = crate::materialization::WorkspaceLock::acquire(workspace_root)?;
    let runtime = crate::local::runtime()?;
    let mut workspace = workspace.clone();
    runtime.block_on(crate::materialization::recover_locked(
        workspace_root,
        repository,
        &mut workspace,
    ))?;
    scan_locked(
        workspace_root,
        repository,
        &workspace,
        max_inline_text_bytes,
        filter,
    )
}

/// Compares the tracked files `filter` selects with the workspace. Unselected files are
/// neither read nor reported, though they stay in `tracked_paths`.
#[tracing::instrument(name = "workspace_scan", level = "debug", skip_all)]
pub fn scan_locked(
    workspace_root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
    max_inline_text_bytes: usize,
    filter: &TreePathFilter,
) -> Result<WorkspaceScan> {
    let runtime = crate::local::runtime()?;
    anyhow::ensure!(
        workspace.store.store_id == repository.store_id,
        "workspace store identity does not match the opened store"
    );
    let materialized = workspace
        .materialized
        .as_ref()
        .context("Wheat workspace has not been materialized")?;
    let materialized_head: RepositoryHead = materialized.repository_head.into();
    let current_head = repository.heads.read()?;
    let (current_label_binding, files) = runtime.block_on(async {
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        // Authenticate only the selected publication roots. Full retained-history validation is
        // intentionally reserved for `wheat validate`, otherwise status/diff latency grows with
        // history that cannot affect this workspace comparison.
        let materialized_envelope = reader.envelope(materialized_head.envelope).await?;
        anyhow::ensure!(
            materialized_envelope.id() == materialized_head.envelope,
            "materialized repository envelope identity mismatch"
        );
        let current_envelope =
            if needs_current_envelope_validation(materialized_head.envelope, current_head.envelope)
            {
                let envelope = reader.envelope(current_head.envelope).await?;
                anyhow::ensure!(
                    envelope.id() == current_head.envelope,
                    "current repository envelope identity mismatch"
                );
                envelope
            } else {
                materialized_envelope
            };
        let labels = reader.view_label_manifest(current_envelope.labels).await?;
        let current_label_binding = labels
            .labels
            .get(&workspace.active_label)
            .map(|label| label.view);
        let files = MerkleTreeStore::new(&repository.objects)
            .walk_files(materialized.tree, TreeWalkLimits::default())
            .await?;
        Ok::<_, anyhow::Error>((current_label_binding, files))
    })?;

    let mut read_budget = ScanReadBudget::new(MAX_TOTAL_READ_BYTES);
    let mut changes = Vec::new();
    let mut tracked_paths = std::collections::BTreeSet::new();
    for file in files {
        tracked_paths.insert(file.path.clone());
        if !filter.matches(&file.path) {
            continue;
        }
        let baseline_limit = read_budget.next_limit(MAX_BASELINE_INSPECTION_BYTES)?;
        // Retained content is collected only when it fits the text-diff eligibility bound.
        // Larger retained leaves are described from their authenticated identity and known size
        // without allocating their contents. Workspace leaves are always streamed below.
        let (baseline, baseline_bytes) = match repository
            .content
            .get_bounded(file.value.content, baseline_limit)
        {
            Ok(bytes) => {
                read_budget.charge(bytes.len())?;
                (
                    descriptor(file.value.kind, file.value.content, &bytes),
                    Some(bytes),
                )
            }
            Err(ContentStoreError::TooLarge { minimum, .. }) => (
                DiffDescriptor {
                    kind: file.value.kind,
                    content: file.value.content,
                    bytes: minimum,
                    text: None,
                },
                None,
            ),
            Err(error) => return Err(error.into()),
        };
        let workspace_limit = read_budget.next_limit(MAX_WORKSPACE_FILE_BYTES)?;
        let observed = absent_where_shape_changed(analyze_workspace_file(
            workspace_root,
            Path::new(&file.path),
            workspace_limit,
            max_inline_text_bytes,
        ))
        .with_context(|| format!("inspect tracked workspace path {}", file.path))?;
        let Some(observed) = observed else {
            let baseline_text = inline_text(baseline.text, baseline_bytes, max_inline_text_bytes);
            changes.push(ScannedChange {
                path: file.path,
                content_changed: true,
                kind_changed: false,
                baseline,
                workspace: None,
                baseline_text,
                workspace_text: None,
            });
            continue;
        };
        read_budget.charge(observed.read_bytes)?;
        let content_changed = observed.content != file.value.content;
        let kind_changed = observed.kind != file.value.kind;
        if content_changed || kind_changed {
            let workspace_descriptor = DiffDescriptor {
                kind: observed.kind,
                content: observed.content,
                bytes: observed.bytes,
                text: Some(observed.text),
            };
            let baseline_text = inline_text(baseline.text, baseline_bytes, max_inline_text_bytes);
            let workspace_text = observed.retained_text;
            changes.push(ScannedChange {
                path: file.path,
                content_changed,
                kind_changed,
                baseline,
                workspace: Some(workspace_descriptor),
                baseline_text,
                workspace_text,
            });
        }
    }
    Ok(WorkspaceScan {
        materialized_view: materialized.view,
        materialized_head_generation: materialized_head.generation,
        current_head_generation: current_head.generation,
        authority_advanced: materialized_head != current_head,
        active_label: workspace.active_label.clone(),
        basis: workspace.basis,
        observed_label_binding: workspace.observed_label_binding,
        current_label_binding,
        label_binding_drifted: current_label_binding != Some(workspace.observed_label_binding),
        selected_frontier: workspace.selected_frontier,
        effective_view: workspace.effective_view(),
        changes,
        tracked_paths,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvelopeValidationPlan {
    PinnedOnly,
    PinnedAndCurrent,
}

impl EnvelopeValidationPlan {
    fn for_heads(
        materialized: wheat_core::RepositoryEnvelopeId,
        current: wheat_core::RepositoryEnvelopeId,
    ) -> Self {
        if materialized == current {
            Self::PinnedOnly
        } else {
            Self::PinnedAndCurrent
        }
    }

    #[cfg(test)]
    fn validation_count(self) -> usize {
        match self {
            Self::PinnedOnly => 1,
            Self::PinnedAndCurrent => 2,
        }
    }
}

fn needs_current_envelope_validation(
    materialized: wheat_core::RepositoryEnvelopeId,
    current: wheat_core::RepositoryEnvelopeId,
) -> bool {
    EnvelopeValidationPlan::for_heads(materialized, current)
        == EnvelopeValidationPlan::PinnedAndCurrent
}

struct ScanReadBudget {
    remaining: usize,
}

impl ScanReadBudget {
    fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    fn next_limit(&self, per_file_limit: usize) -> Result<usize> {
        anyhow::ensure!(
            self.remaining != 0,
            "tracked workspace scan exhausted its aggregate byte limit"
        );
        Ok(self.remaining.min(per_file_limit))
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .context("tracked workspace scan exceeded its aggregate byte limit")?;
        Ok(())
    }
}

fn descriptor(kind: FileKind, content: ContentId, bytes: &[u8]) -> DiffDescriptor {
    DiffDescriptor {
        kind,
        content,
        bytes: bytes.len(),
        text: Some(
            kind != FileKind::Symlink
                && text_capture::classify(bytes) == text_capture::CaptureKind::Text,
        ),
    }
}

fn inline_text(text: Option<bool>, bytes: Option<Vec<u8>>, limit: usize) -> Option<Vec<u8>> {
    bytes.filter(|bytes| text == Some(true) && bytes.len() <= limit)
}

/// Fails when the workspace has tracked edits that weren't captured.
pub fn ensure_workspace_clean_locked(
    root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
) -> Result<()> {
    let scan = scan_locked(root, repository, workspace, 0, &TreePathFilter::default())?;
    if !scan.changes.is_empty() {
        return Err(crate::outcome::CliFailure::invalid(
            "uncaptured tracked workspace edits block switch, refresh or integrate",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_remaining_is_the_bound_for_each_next_side() {
        let mut budget = ScanReadBudget::new(10);
        assert_eq!(budget.next_limit(6).unwrap(), 6);
        budget.charge(6).unwrap();
        assert_eq!(budget.next_limit(6).unwrap(), 4);
        budget.charge(4).unwrap();
        assert!(budget.next_limit(6).is_err());
        assert!(budget.charge(1).is_err());
    }

    #[test]
    fn identical_envelopes_skip_the_second_full_validation() {
        let envelope = wheat_core::RepositoryEnvelopeId::from_digest([7; 32]);
        let other = wheat_core::RepositoryEnvelopeId::from_digest([8; 32]);
        let same = EnvelopeValidationPlan::for_heads(envelope, envelope);
        let distinct = EnvelopeValidationPlan::for_heads(envelope, other);
        assert_eq!(same.validation_count(), 1);
        assert_eq!(distinct.validation_count(), 2);
        assert!(!needs_current_envelope_validation(envelope, envelope));
        assert!(needs_current_envelope_validation(envelope, other));
    }
}
