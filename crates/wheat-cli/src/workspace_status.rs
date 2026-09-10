use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use wheat_core::{ContentId, FileKind, ViewId, text_capture};
use wheat_store::{
    MerkleTreeStore, PublicationStore, RepositoryHead, RepositoryReader, TreeWalkLimits,
};

use crate::{
    local::LocalRepository, workspace_files::read_workspace_file,
    workspace_metadata::WorkspaceMetadata,
};

const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

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
    pub baseline: FileDescriptor,
    pub workspace: Option<FileDescriptor>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FileDescriptor {
    pub kind: FileKind,
    pub content: ContentId,
    pub bytes: usize,
    pub text: bool,
}

pub(crate) fn inspect(
    workspace_root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
) -> Result<WorkspaceStatus> {
    let _lock = crate::materialization::WorkspaceLock::acquire(workspace_root)?;
    let runtime = crate::local::runtime()?;
    let mut workspace = workspace.clone();
    runtime.block_on(crate::materialization::recover(
        workspace_root,
        repository,
        &mut workspace,
    ))?;
    anyhow::ensure!(
        workspace.store.store_id == repository.store_id,
        "workspace store identity does not match the opened store"
    );
    let materialized = workspace
        .materialized
        .as_ref()
        .context("Wheat workspace has not been materialized")?;
    let current_head = repository.heads.read()?;
    let (state, files) = runtime.block_on(async {
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.validate_envelope(current_head.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let files = MerkleTreeStore::new(&repository.objects)
            .walk_files(materialized.tree, TreeWalkLimits::default())
            .await?;
        Ok::<_, anyhow::Error>((state, files))
    })?;

    let mut total = 0usize;
    let mut changes = Vec::new();
    for file in files {
        let baseline_bytes = repository.content.get(file.value.content)?;
        total = total
            .checked_add(baseline_bytes.len())
            .context("tracked workspace status byte count overflow")?;
        if total > MAX_TOTAL_BYTES {
            anyhow::bail!("tracked workspace status exceeded {MAX_TOTAL_BYTES} bytes");
        }
        let baseline = descriptor(file.value.kind, file.value.content, &baseline_bytes);
        let observed = read_workspace_file(workspace_root, Path::new(&file.path), MAX_FILE_BYTES)
            .with_context(|| format!("inspect tracked workspace path {}", file.path))?;
        let Some(observed) = observed else {
            changes.push(TrackedChange {
                path: file.path,
                content_changed: true,
                kind_changed: false,
                deleted: true,
                baseline,
                workspace: None,
            });
            continue;
        };
        total = total
            .checked_add(observed.bytes.len())
            .context("tracked workspace status byte count overflow")?;
        if total > MAX_TOTAL_BYTES {
            anyhow::bail!("tracked workspace status exceeded {MAX_TOTAL_BYTES} bytes");
        }
        let canonical = canonical_bytes(observed.kind, &observed.bytes);
        let observed_id = ContentId::of(canonical.as_ref());
        let content_changed = observed_id != file.value.content;
        let kind_changed = observed.kind != file.value.kind;
        if content_changed || kind_changed {
            changes.push(TrackedChange {
                path: file.path,
                content_changed,
                kind_changed,
                deleted: false,
                baseline,
                workspace: Some(descriptor(observed.kind, observed_id, canonical.as_ref())),
            });
        }
    }

    let materialized_head: RepositoryHead = materialized.repository_head.clone().into();
    Ok(WorkspaceStatus {
        schema_version: 1,
        materialized_view: materialized.view,
        materialized_head_generation: materialized_head.generation,
        current_head_generation: current_head.generation,
        authority_advanced: materialized_head != current_head,
        working_view: state.working_view,
        published_view: state.published_view,
        changes,
    })
}

fn canonical_bytes<'a>(kind: FileKind, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
    if kind == FileKind::Symlink {
        return std::borrow::Cow::Borrowed(bytes);
    }
    match text_capture::capture(bytes) {
        text_capture::CapturedBytes::Text(text) => match text {
            std::borrow::Cow::Borrowed(text) => std::borrow::Cow::Borrowed(text.as_bytes()),
            std::borrow::Cow::Owned(text) => std::borrow::Cow::Owned(text.into_bytes()),
        },
        text_capture::CapturedBytes::Binary(binary) => binary,
    }
}

fn descriptor(kind: FileKind, content: ContentId, bytes: &[u8]) -> FileDescriptor {
    FileDescriptor {
        kind,
        content,
        bytes: bytes.len(),
        text: kind != FileKind::Symlink
            && matches!(
                text_capture::capture(bytes),
                text_capture::CapturedBytes::Text(_)
            ),
    }
}
