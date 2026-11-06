//! One-pass comparison of the workspace with its pinned materialized tree.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::Instrument as _;
use wheat_core::{
    ContentId, FileKind, FileValue, ViewId, diff_model::DiffDescriptor, text_capture,
};
use wheat_store::{
    ContentStoreError, MerkleTreeStore, PublicationStore, RepositoryHead, RepositoryReader,
    TreePathFilter, TreeWalkLimits,
};

use crate::{
    local::LocalRepository,
    stat_cache::{Observed, StatCache},
    workspace_files::{
        FileStamp, SAMPLED_FILE_BYTES, WorkspaceStats, absent_where_shape_changed,
        analyze_workspace_file, sample_workspace_file,
    },
    workspace_metadata::WorkspaceMetadata,
};

/// Whether a scan must give the content ID of each file it finds modified. When it
/// need not, a long binary file whose samples differ from those it had when it was
/// last found unchanged is reported modified without reading the rest of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentIds {
    Required,
    Optional,
}

/// Status reads a file of any length the format stores; `.wheat.json`'s limit is for
/// capture. Nothing caps the bytes a scan reads in all: it streams each file, so what
/// it holds does not grow with them, and a large workspace's first scan reads it all.
const MAX_WORKSPACE_FILE_BYTES: usize = crate::repository_config::FORMAT_MAX_FILE_BYTES;
const MAX_BASELINE_INSPECTION_BYTES: usize = 1024 * 1024;

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
    pub reads: ScanReads,
}

/// What a scan read to answer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanReads {
    /// Workspace files read whole.
    pub files_read: usize,
    /// Their raw bytes.
    pub bytes_read: u64,
    /// Long workspace files whose samples were read instead.
    pub files_sampled: usize,
    /// Baseline contents looked up in the store.
    pub baselines_read: usize,
}

#[derive(Debug)]
pub struct ScannedChange {
    pub path: String,
    pub content_changed: bool,
    pub kind_changed: bool,
    pub deleted: bool,
    pub baseline: DiffDescriptor,
    /// The workspace's file; none when it is deleted, or when it was found modified
    /// by its samples under [`ContentIds::Optional`].
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
    ids: ContentIds,
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
        ids,
    )
}

/// Compares the tracked files `filter` selects with the workspace. Unselected files are
/// neither read nor reported, though they stay in `tracked_paths`. A file whose stat
/// is what the workspace's stat cache (`.wheat/stat.sqlite`) recorded is not read
/// again; nor is its baseline when it is unchanged.
#[tracing::instrument(name = "workspace_scan", level = "debug", skip_all)]
pub fn scan_locked(
    workspace_root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
    max_inline_text_bytes: usize,
    filter: &TreePathFilter,
    ids: ContentIds,
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
            .instrument(tracing::debug_span!("walk_tracked_files"))
            .await?;
        Ok::<_, anyhow::Error>((current_label_binding, files))
    })?;

    let mut cache = StatCache::open(workspace_root)?;
    let mut reads = ScanReads::default();
    let mut changes = Vec::new();
    let mut tracked_paths = std::collections::BTreeSet::new();
    let check = tracing::debug_span!("check_tracked_files").entered();
    let mut stats = WorkspaceStats::new(workspace_root)
        .with_context(|| format!("open workspace {}", workspace_root.display()))?;
    for file in files {
        tracked_paths.insert(file.path.clone());
        if !filter.matches(&file.path) {
            continue;
        }
        let path = Path::new(&file.path);
        let stamp = absent_where_shape_changed(stats.stat(path))
            .with_context(|| format!("inspect tracked workspace path {}", file.path))?;
        let cached = match stamp {
            Some(_) => cache.get(&file.path).copied(),
            None => None,
        };
        if let (Some(stamp), Some((cached_stamp, cached))) = (stamp, cached) {
            if cached_stamp == stamp {
                if is_unchanged(&cached, file.value) {
                    continue;
                }
                // A file still modified as it was: what it holds is known, but an inline
                // diff needs its text.
                if max_inline_text_bytes == 0 {
                    let (baseline, baseline_bytes) =
                        read_baseline(repository, file.value, &mut reads)?;
                    changes.push(ScannedChange {
                        path: file.path,
                        content_changed: cached.content != file.value.content,
                        kind_changed: cached.kind != file.value.kind,
                        deleted: false,
                        baseline_text: inline_text(baseline.text, baseline_bytes, 0),
                        baseline,
                        workspace: Some(DiffDescriptor {
                            kind: cached.kind,
                            content: cached.content,
                            bytes: cached.bytes,
                            text: Some(cached.text),
                        }),
                        workspace_text: None,
                    });
                    continue;
                }
            } else if ids == ContentIds::Optional
                && samples_differ(
                    workspace_root,
                    path,
                    &stamp,
                    &cached_stamp,
                    &cached,
                    file.value,
                    &mut reads,
                )?
            {
                let (baseline, baseline_bytes) = read_baseline(repository, file.value, &mut reads)?;
                changes.push(ScannedChange {
                    path: file.path,
                    content_changed: true,
                    kind_changed: false,
                    deleted: false,
                    baseline_text: inline_text(baseline.text, baseline_bytes, 0),
                    baseline,
                    workspace: None,
                    workspace_text: None,
                });
                continue;
            }
        }
        let (baseline, baseline_bytes) = read_baseline(repository, file.value, &mut reads)?;
        let observed = absent_where_shape_changed(analyze_workspace_file(
            workspace_root,
            Path::new(&file.path),
            MAX_WORKSPACE_FILE_BYTES,
            max_inline_text_bytes,
        ))
        .with_context(|| format!("inspect tracked workspace path {}", file.path))?;
        let Some(observed) = observed else {
            let baseline_text = inline_text(baseline.text, baseline_bytes, max_inline_text_bytes);
            changes.push(ScannedChange {
                path: file.path,
                content_changed: true,
                kind_changed: false,
                deleted: true,
                baseline,
                workspace: None,
                baseline_text,
                workspace_text: None,
            });
            continue;
        };
        reads.files_read += 1;
        reads.bytes_read += observed.read_bytes as u64;
        if let Some(stamp) = observed.stamp {
            cache.record(
                &file.path,
                stamp,
                Observed {
                    kind: observed.kind,
                    content: observed.content,
                    bytes: observed.bytes,
                    text: observed.text,
                    samples: observed.samples,
                },
            );
        }
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
                deleted: false,
                baseline,
                workspace: Some(workspace_descriptor),
                baseline_text,
                workspace_text,
            });
        }
    }
    drop(check);
    cache.finish(filter.is_everything().then_some(&tracked_paths))?;
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
        reads,
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

/// Whether what the stat cache recorded is the baseline.
fn is_unchanged(cached: &Observed, baseline: FileValue) -> bool {
    cached.kind == baseline.kind && cached.content == baseline.content
}

/// Whether the long binary file at `path`, whose stat changed from `cached_stamp` to
/// `stamp`, is shown modified by its length or its samples, which were recorded when
/// it held the baseline. False when that cannot be told without reading it whole.
fn samples_differ(
    root: &Path,
    path: &Path,
    stamp: &FileStamp,
    cached_stamp: &FileStamp,
    cached: &Observed,
    baseline: FileValue,
    reads: &mut ScanReads,
) -> Result<bool> {
    let Some(samples) = cached.samples else {
        return Ok(false);
    };
    if !is_unchanged(cached, baseline)
        || cached.text
        || stamp.kind() != cached.kind
        || stamp.size < SAMPLED_FILE_BYTES
    {
        return Ok(false);
    }
    // Binary content is its raw bytes, so another length is other content.
    if stamp.size != cached_stamp.size {
        return Ok(true);
    }
    reads.files_sampled += 1;
    let now = sample_workspace_file(root, path, stamp)
        .with_context(|| format!("sample tracked workspace path {}", path.display()))?;
    Ok(now.is_some_and(|now| now != samples))
}

/// The baseline's descriptor, with its bytes when they fit the text-diff bound. Longer
/// content is described from its identity and stored length without reading it.
fn read_baseline(
    repository: &LocalRepository,
    value: FileValue,
    reads: &mut ScanReads,
) -> Result<(DiffDescriptor, Option<Vec<u8>>)> {
    reads.baselines_read += 1;
    match repository
        .content
        .get_bounded(value.content, MAX_BASELINE_INSPECTION_BYTES)
    {
        Ok(bytes) => Ok((descriptor(value.kind, value.content, &bytes), Some(bytes))),
        Err(ContentStoreError::TooLarge { minimum, .. }) => Ok((
            DiffDescriptor {
                kind: value.kind,
                content: value.content,
                bytes: minimum,
                text: None,
            },
            None,
        )),
        Err(error) => Err(error.into()),
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
    let scan = scan_locked(
        root,
        repository,
        workspace,
        0,
        &TreePathFilter::default(),
        ContentIds::Optional,
    )?;
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
