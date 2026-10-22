//! Crash-recoverable materialization of a separate-store workspace.
//!
//! The immutable journal records only the old-to-target tree delta. Every changed
//! workspace leaf is authenticated against its exact raw old value before the
//! journal is published. Installation then uses descriptor-relative, no-follow
//! operations: creates use `RENAME_NOREPLACE`, replacements use
//! `RENAME_EXCHANGE`, and deletions exchange through a private tombstone. Recovery
//! derives state from the authenticated workspace and transaction leaves and
//! refuses every ambiguous state.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    FileKind, FileValue, MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE,
    MaterializationProgressFields, TreeId, ViewId, decode_materialization_progress_record,
    encode_materialization_progress_fields, encode_materialization_progress_record,
};
use wheat_ignore::WORKSPACE_CONTROL_NAMES as RESERVED;
use wheat_store::{
    ContentStore, MerkleTreeStore, PublicationStore, RepositoryHead, TreeDiffLimits, TreeFile,
    TreeWalkLimits,
};

use crate::{
    dot_git::DotGitGate,
    local::LocalRepository,
    sys,
    workspace_files::{
        RawPreimage, WorkspaceFileError, WorkspaceFileSnapshot, authenticate_preimage_at,
        authenticate_workspace_file, authenticate_workspace_preimage, observe_workspace_file,
        raw_preimage, read_workspace_file,
    },
    workspace_metadata::{MaterializedState, WorkspaceMetadata, WorkspaceMetadataPublishError},
};

const VERSION: u32 = 4;
const JOURNAL_NAME: &str = "materialization.json";
const TRANSACTIONS: &str = "materialization-transactions";
const PROGRESS_NAME: &str = "progress.log";
const PROGRESS_RECORD_BYTES: usize = MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE;
const PROGRESS_OPCODE_INSTALL_INTENT: u16 = 1;
const PROGRESS_OPCODE_COMPLETE: u16 = 2;
const PROGRESS_OPCODE_ABORTED: u16 = 3;
const MAX_JOURNAL_BYTES: usize = 192 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
// ContentStore currently returns owned Vecs. A replacement needs old and new
// bytes concurrently to authenticate both sides of an exchange; workspace,
// staged, and displaced regular files are streamed through a fixed 64 KiB
// scratch buffer and add no file-sized allocation. This bounds one install's
// owned file content.
const MAX_OPERATION_RESIDENT_BYTES: u64 = 2 * MAX_FILE_BYTES as u64;
/// The most installs staged, recorded and synced together. An abort looks for
/// staged but unrecorded leaves only this far past the recorded intents.
const MAX_BATCH_INSTALLS: usize = 256;
/// A batch keeps its installs' store buffers from staging until they are applied,
/// and is closed before its buffers would pass this bound; an install larger than
/// it is a batch of its own. The install that closed a batch stays loaded for the
/// next one, so peak owned file content is at most this plus one install. When this
/// was `MAX_OPERATION_RESIDENT_BYTES`, cloning 32 files of 12 MiB held all 384 MiB
/// at once (peak RSS 430–513 MB); with 64 MiB it peaks at 98 MiB (2026-10-15,
/// publish_content).
const MAX_BATCH_RESIDENT_BYTES: u64 = 64 * 1024 * 1024;
/// Threads that write and sync a batch's staged files, so the filesystem can
/// commit several syncs at once. Importing 4,096 small files on NVMe ext4 took
/// 3.7 s with 8, 1.3 s with 32 and 1.3 s with 64 (2026-10-05).
const STAGING_WORKERS: usize = 32;
#[cfg(test)]
static TEST_PEAK_OWNED_CONTENT_BYTES: AtomicUsize = AtomicUsize::new(0);
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TEST_CHECKPOINT_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_AFTER";
const TEST_CHECKPOINT_OCCURRENCE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_OCCURRENCE";
const TEST_CHECKPOINT_MODE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_MODE";
const TEST_PAUSE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_PAUSE_AT";
const TEST_PAUSE_FILE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_PAUSE_FILE";
const MAX_ORPHAN_TRANSACTIONS: usize = 1_024;
const MAX_SELECTED_FILES: usize = 250_000;
const MAX_SELECTORS: usize = 250_000;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_TOTAL_PATH_BYTES: usize = 64 * 1024 * 1024;
static CHECKPOINT_OCCURRENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Error)]
#[error("{message}")]
struct RestoreInvalidated {
    message: String,
}

fn restore_invalidated(message: impl Into<String>) -> anyhow::Error {
    RestoreInvalidated {
        message: message.into(),
    }
    .into()
}

fn invalidated_install(install: &Install, message: String) -> anyhow::Error {
    if install.restore_consent {
        restore_invalidated(message)
    } else {
        anyhow::anyhow!(message)
    }
}

/// Report whether the workspace leaf at `path` is exactly `value`: the same kind and raw
/// bytes that materializing `value` would write, or an absent path when `value` is `None`.
///
/// Obstructions that materialization would refuse (an intermediate symlink, an unsupported
/// file type, or an oversized leaf) are reported as a mismatch rather than an error.
pub fn workspace_holds(
    root: &Path,
    content: &ContentStore,
    path: &str,
    value: Option<FileValue>,
) -> Result<bool> {
    let Some(value) = value else {
        return path_is_absent(root, path);
    };
    let bytes = content.get(value.content)?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "materialized file exceeds byte limit"
    );
    match authenticate_workspace_file(
        root,
        Path::new(path),
        Some((value.kind, &bytes)),
        MAX_FILE_BYTES,
    ) {
        Ok(matches) => Ok(matches),
        Err(
            WorkspaceFileError::IntermediateSymlink
            | WorkspaceFileError::AncestorNotDirectory
            | WorkspaceFileError::UnsupportedFileType
            | WorkspaceFileError::TooLarge { .. },
        ) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn path_is_absent(root: &Path, path: &str) -> Result<bool> {
    match observe_workspace_file(root, Path::new(path), 0) {
        Ok(None) => Ok(true),
        Ok(Some(_))
        | Err(
            WorkspaceFileError::UnsupportedFileType
            | WorkspaceFileError::AncestorNotDirectory
            | WorkspaceFileError::TooLarge { .. }
            | WorkspaceFileError::IntermediateSymlink,
        ) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn is_shadowed_by_exact_obstruction(
    path: &str,
    obstructions: &HashSet<&str>,
    mut inspected_ancestor: impl FnMut(&str),
) -> bool {
    let mut parent = Path::new(path).parent();
    while let Some(relative_path) = parent {
        let Some(relative) = relative_path.to_str().filter(|path| !path.is_empty()) else {
            break;
        };
        inspected_ancestor(relative);
        if obstructions.contains(relative) {
            return true;
        }
        parent = relative_path.parent();
    }
    false
}

fn descendant_representatives<'a>(
    selectors: &'a [String],
    selected: &'a std::collections::BTreeMap<String, FileValue>,
    mut inspected_ancestor: impl FnMut(&str),
) -> HashMap<&'a str, FileValue> {
    let selector_index = selectors.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut representatives = HashMap::with_capacity(selectors.len().min(selected.len()));
    for (path, value) in selected {
        let mut parent = Path::new(path).parent();
        while let Some(relative_path) = parent {
            let Some(relative) = relative_path.to_str().filter(|path| !path.is_empty()) else {
                break;
            };
            inspected_ancestor(relative);
            if selector_index.contains(relative) {
                representatives.entry(relative).or_insert(*value);
            }
            parent = relative_path.parent();
        }
    }
    representatives
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    path: String,
    old: Option<FileValue>,
    new: Option<FileValue>,
    /// Restore consent authenticates the exact raw value observed before the
    /// immutable journal was published, rather than merely the pinned value.
    observed: Option<RawPreimage>,
    restore_consent: bool,
    /// The exact selected leaf was obstructed by an observed empty directory.
    empty_directory: bool,
    staged: String,
    tombstone: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Materialize,
    Restore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    operation: Operation,
    transaction_id: Uuid,
    workspace_id: Uuid,
    store_id: Uuid,
    source: Option<MaterializedState>,
    active_label: String,
    basis: ViewId,
    observed_label_binding: ViewId,
    selected_frontier: Option<ViewId>,
    /// Canonical selectors authorizing a restore; empty for materialization.
    selectors: Vec<String>,
    target: MaterializedState,
    installs: Vec<Install>,
}

/// The authenticated descendants and operation boundary for one directory-to-file
/// transition. Both sets make directory inventory membership checks constant-time.
/// Paths borrow slices of the authenticated installs so deep prefixes are not copied.
struct ReplacedDirectoryPlan<'a> {
    expected_leaves: HashSet<&'a str>,
    expected_directories: HashSet<&'a str>,
    first_descendant_operation: usize,
}

/// Structural facts derived once from an authenticated plan. Keeping these indices
/// outside the per-install path makes flat plans O(K log K), while ancestor work is
/// bounded by path depth rather than by the number of installs. Every indexed path
/// borrows an install path allocation, avoiding quadratic bytes for deep prefixes.
struct PlannerContext<'a> {
    removed_paths: BTreeSet<&'a str>,
    replaced_directories: HashMap<&'a str, ReplacedDirectoryPlan<'a>>,
    source_directories: HashSet<&'a str>,
    target_directories: HashSet<&'a str>,
}

impl<'a> PlannerContext<'a> {
    fn new(installs: &'a [Install]) -> Self {
        Self::new_with_replacement_lookup(installs, |_| {})
    }

    fn new_with_replacement_lookup(
        installs: &'a [Install],
        mut inspected_ancestor: impl FnMut(&str),
    ) -> Self {
        let removed_paths = installs
            .iter()
            .filter(|install| install.old.is_some() && install.new.is_none())
            .map(|install| install.path.as_str())
            .collect::<BTreeSet<_>>();
        let replacement_operations = installs
            .iter()
            .enumerate()
            .filter(|(_, install)| install.old.is_none() && install.new.is_some())
            .map(|(index, install)| (install.path.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut replaced_directories = HashMap::<&str, ReplacedDirectoryPlan<'_>>::new();
        for (operation, install) in installs
            .iter()
            .enumerate()
            .filter(|(_, install)| install.old.is_some() && install.new.is_none())
        {
            let mut ancestor = Path::new(&install.path).parent();
            while let Some(relative) = ancestor {
                let Some(relative) = relative.to_str().filter(|path| !path.is_empty()) else {
                    break;
                };
                inspected_ancestor(relative);
                if replacement_operations.contains_key(relative) {
                    let plan = replaced_directories.entry(relative).or_insert_with(|| {
                        ReplacedDirectoryPlan {
                            expected_leaves: HashSet::new(),
                            expected_directories: HashSet::new(),
                            first_descendant_operation: operation,
                        }
                    });
                    plan.expected_leaves.insert(install.path.as_str());
                    plan.first_descendant_operation =
                        plan.first_descendant_operation.min(operation);
                    let mut directory = Path::new(&install.path).parent();
                    while let Some(expected_path) = directory {
                        let Some(expected) = expected_path.to_str() else {
                            break;
                        };
                        if expected == relative {
                            break;
                        }
                        plan.expected_directories.insert(expected);
                        directory = expected_path.parent();
                    }
                    break;
                }
                ancestor = Path::new(relative).parent();
            }
        }

        let mut source_directories = HashSet::new();
        let mut target_directories = HashSet::new();
        for install in installs {
            if install.old.is_some() {
                insert_parent_paths(&install.path, &mut source_directories);
            }
            if install.new.is_some() {
                insert_parent_paths(&install.path, &mut target_directories);
            }
        }
        Self {
            removed_paths,
            replaced_directories,
            source_directories,
            target_directories,
        }
    }

    fn is_shadowed_by_removed_leaf(&self, path: &str) -> bool {
        self.shadowing_removed_leaf(path, |_| {})
    }

    fn shadowing_removed_leaf(&self, path: &str, mut inspected: impl FnMut(&Path)) -> bool {
        let mut parent = Path::new(path).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            inspected(relative);
            if relative
                .to_str()
                .is_some_and(|path| self.removed_paths.contains(path))
            {
                return true;
            }
            parent = relative.parent();
        }
        false
    }
}

fn insert_parent_paths<'a>(path: &'a str, directories: &mut HashSet<&'a str>) {
    let mut parent = Path::new(path).parent();
    while let Some(relative) = parent {
        if relative.as_os_str().is_empty() {
            break;
        }
        // Install paths have already been validated as UTF-8 strings, and every
        // parent produced here is a byte-aligned slice of that same string.
        directories.insert(relative.to_str().expect("parent of UTF-8 install path"));
        parent = relative.parent();
    }
}

pub struct WorkspaceLock {
    lock: File,
    control: File,
}

impl WorkspaceLock {
    #[tracing::instrument(name = "workspace_lock", level = "debug", skip_all)]
    pub fn acquire(root: &Path) -> Result<Self> {
        let control = open_directory(&root.join(".wheat"))?;
        let file = sys::open_file_at(
            &control,
            "workspace.lock",
            sys::OpenFile {
                read: true,
                write: true,
                append: false,
                creation: sys::Creation::IfMissing,
                mode: 0o600,
            },
        )
        .context("open Wheat workspace lock")?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("workspace is locked by another operation");
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(Self {
            lock: file,
            control,
        })
    }

    pub fn control(&self) -> &File {
        &self.control
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

pub async fn materialize(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
    head: RepositoryHead,
    active_label: String,
    basis: ViewId,
    selected_frontier: Option<ViewId>,
    tree: TreeId,
    dot_git: DotGitGate,
) -> Result<()> {
    let _lock = WorkspaceLock::acquire(root)?;
    materialize_locked(
        root,
        repository,
        metadata,
        head,
        active_label,
        basis,
        selected_frontier,
        tree,
        dot_git,
    )
    .await
}

pub async fn materialize_locked(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
    head: RepositoryHead,
    active_label: String,
    basis: ViewId,
    selected_frontier: Option<ViewId>,
    tree: TreeId,
    dot_git: DotGitGate,
) -> Result<()> {
    let persisted = WorkspaceMetadata::read(root)?;
    ensure!(
        persisted.workspace_id == metadata.workspace_id
            && persisted.store == metadata.store
            && persisted.store.store_id == repository.store_id,
        "workspace/store identity changed while materialization was resolved"
    );
    *metadata = persisted;
    recover_locked(root, repository, metadata).await?;

    let view = selected_frontier.unwrap_or(basis);
    let target = MaterializedState {
        repository_head: head.into(),
        view,
        tree,
    };
    if metadata.materialized.as_ref() == Some(&target)
        && metadata.active_label == active_label
        && metadata.basis == basis
        && metadata.observed_label_binding == basis
        && metadata.selected_frontier == selected_frontier
    {
        ensure!(
            repository.heads.read()? == head,
            "repository head changed while materialization was resolved"
        );
        return Ok(());
    }

    let transaction_id = Uuid::now_v7();
    let source = metadata.materialized.clone();
    let installs = if let Some(current) = &source {
        plan_delta(repository, current.tree, tree, transaction_id).await?
    } else {
        let files = target_files(repository, tree).await?;
        validate_materializable(repository, &files)?;
        validate_initial_workspace(root, &files)?;
        files
            .into_iter()
            .enumerate()
            .map(|(index, file)| Install {
                path: file.path,
                old: None,
                new: Some(file.value),
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: format!("leaf-{index:08x}"),
                tombstone: None,
            })
            .collect()
    };
    let installs = preflight_materialization(root, repository, installs)?;
    // Before the journal exists, so a refusal leaves the workspace untouched.
    dot_git.check(writes(&installs))?;
    let journal = Journal {
        version: VERSION,
        operation: Operation::Materialize,
        transaction_id,
        workspace_id: metadata.workspace_id,
        store_id: metadata.store.store_id,
        source,
        active_label,
        basis,
        observed_label_binding: basis,
        selected_frontier,
        selectors: Vec::new(),
        target,
        installs,
    };
    let planner = PlannerContext::new(&journal.installs);
    checkpoint("preflight-complete")?;
    ensure!(
        repository.heads.read()? == head,
        "repository head changed while materialization was planned"
    );
    let journal_bytes = encode_journal(&journal)?;
    crate::workspace_registry_service::transition_materialization(
        root,
        &metadata.store.path,
        metadata,
        head,
        view,
        tree,
    )?;
    let (tx, mut progress) = match create_transaction(root, transaction_id) {
        Ok(transaction) => transaction,
        Err(error) => {
            crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)?;
            return Err(error);
        }
    };
    if let Err(error) = write_journal(root, &journal_bytes) {
        if !journal_exists(root)? {
            cleanup_abandoned_transaction(root, transaction_id, &tx)?;
            crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)?;
        }
        return Err(error);
    }
    checkpoint("journal-created")?;
    let complete = roll_forward(root, repository, &tx, &mut progress, &journal, &planner)?;
    checkpoint("all-installed")?;
    publish_target(root, metadata, &journal)?;
    checkpoint("target-published")?;
    if !complete {
        append_completion(
            &mut progress,
            journal.transaction_id,
            journal.installs.len(),
        )?;
        checkpoint("completion-persisted")?;
    }
    cleanup_committed(root, repository, &journal, &tx)?;
    crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)?;
    Ok(())
}

pub async fn restore(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
    selectors: &[String],
    dot_git: DotGitGate,
) -> Result<usize> {
    let _lock = WorkspaceLock::acquire(root)?;
    let persisted = WorkspaceMetadata::read(root)?;
    ensure!(
        persisted.workspace_id == metadata.workspace_id
            && persisted.store == metadata.store
            && persisted.store.store_id == repository.store_id,
        "workspace/store identity changed while restore was resolved"
    );
    *metadata = persisted;
    recover_locked(root, repository, metadata).await?;
    let pinned = metadata
        .materialized
        .clone()
        .context("workspace has no pinned materialized tree")?;
    ensure!(
        !selectors.is_empty(),
        "at least one restore path is required"
    );

    let mut normalized_selectors = selectors.to_vec();
    normalized_selectors.sort_unstable();
    normalized_selectors.dedup();
    validate_selectors(&normalized_selectors)?;
    let selected = expand_restore_selectors(repository, pinned.tree, &normalized_selectors).await?;

    let transaction_id = Uuid::now_v7();
    let mut installs = Vec::with_capacity(selected.len() + normalized_selectors.len());
    let representatives = descendant_representatives(&normalized_selectors, &selected, |_| {});
    let mut structural_obstructions = HashSet::new();
    for selector in &normalized_selectors {
        let Some(representative) = representatives.get(selector.as_str()) else {
            continue;
        };
        match observe_workspace_file(root, Path::new(selector), MAX_FILE_BYTES) {
            Ok(Some(observed)) => {
                structural_obstructions.insert(selector.as_str());
                let index = installs.len();
                installs.push(Install {
                    path: selector.clone(),
                    old: Some(*representative),
                    new: None,
                    observed: Some(observed),
                    restore_consent: true,
                    empty_directory: false,
                    staged: format!("leaf-{index:08x}"),
                    tombstone: Some(
                        format!("wheat-delete-v1:{transaction_id}:{index:08x}").into_bytes(),
                    ),
                });
            }
            Ok(None)
            | Err(
                WorkspaceFileError::UnsupportedFileType | WorkspaceFileError::AncestorNotDirectory,
            ) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("observe restore structure {selector:?}"));
            }
        }
    }
    let mut total = 0_u64;
    for (path, value) in selected {
        let shadowed_by_obstruction =
            is_shadowed_by_exact_obstruction(&path, &structural_obstructions, |_| {});
        let (observed, empty_directory) = if shadowed_by_obstruction {
            (None, false)
        } else {
            match observe_workspace_file(root, Path::new(&path), MAX_FILE_BYTES) {
                Ok(observed) => (observed, false),
                Err(crate::workspace_files::WorkspaceFileError::UnsupportedFileType)
                    if path_is_directory(root, &path)? =>
                {
                    ensure!(
                        directory_is_empty(root, &path)?,
                        "refusing to replace nonempty workspace directory {path:?}"
                    );
                    (None, true)
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("observe restore path {path:?}"));
                }
            }
        };
        let bytes = repository.content.get(value.content)?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "restored file exceeds byte limit"
        );
        if value.kind == FileKind::Symlink {
            ensure!(
                !bytes.is_empty() && !bytes.contains(&0) && bytes.len() <= 4_095,
                "restored symlink target is not representable"
            );
        }
        if observed.as_ref() == Some(&raw_preimage(value.kind, &bytes)) {
            continue;
        }
        total = total
            .checked_add(u64::try_from(bytes.len())?)
            .context("restore byte count overflow")?;
        ensure!(total <= MAX_TOTAL_BYTES, "restore exceeds total byte limit");
        let index = installs.len();
        installs.push(Install {
            path,
            old: observed.as_ref().map(|_| value),
            new: Some(value),
            observed,
            restore_consent: true,
            empty_directory,
            staged: format!("leaf-{index:08x}"),
            tombstone: None,
        });
    }
    if installs.is_empty() {
        return Ok(0);
    }
    installs.sort_by(|left, right| left.path.cmp(&right.path));
    for (index, install) in installs.iter_mut().enumerate() {
        install.staged = format!("leaf-{index:08x}");
        if install.new.is_none() {
            install.tombstone =
                Some(format!("wheat-delete-v1:{transaction_id}:{index:08x}").into_bytes());
        }
    }
    // Before the journal exists, so a refusal leaves the workspace untouched.
    dot_git.check(writes(&installs))?;
    let journal = Journal {
        version: VERSION,
        operation: Operation::Restore,
        transaction_id,
        workspace_id: metadata.workspace_id,
        store_id: metadata.store.store_id,
        source: Some(pinned.clone()),
        active_label: metadata.active_label.clone(),
        basis: metadata.basis,
        observed_label_binding: metadata.observed_label_binding,
        selected_frontier: metadata.selected_frontier,
        selectors: normalized_selectors,
        target: pinned,
        installs,
    };
    let planner = PlannerContext::new(&journal.installs);
    authenticate_preflight(root, repository, &journal.installs, &planner)?;
    checkpoint("preflight-complete")?;
    let journal_bytes = encode_journal(&journal)?;
    let (tx, mut progress) = create_transaction(root, transaction_id)?;
    if let Err(error) = write_journal(root, &journal_bytes) {
        if !journal_exists(root)? {
            cleanup_abandoned_transaction(root, transaction_id, &tx)?;
        }
        return Err(error);
    }
    checkpoint("journal-created")?;
    let complete = match roll_forward(root, repository, &tx, &mut progress, &journal, &planner) {
        Ok(complete) => complete,
        Err(error) if error.downcast_ref::<RestoreInvalidated>().is_some() => {
            abort_restore(root, repository, &tx, &mut progress, &journal)?;
            return Err(error).context(
                "restore plan was invalidated by a changed selected path; preserved the new value",
            );
        }
        Err(error) => return Err(error),
    };
    checkpoint("all-installed")?;
    if !complete {
        append_completion(
            &mut progress,
            journal.transaction_id,
            journal.installs.len(),
        )?;
        checkpoint("completion-persisted")?;
    }
    cleanup_committed(root, repository, &journal, &tx)?;
    Ok(journal
        .installs
        .iter()
        .filter(|install| install.new.is_some())
        .count())
}

/// The paths `installs` writes a file to. Deletions write nothing.
fn writes(installs: &[Install]) -> impl Iterator<Item = &str> {
    installs
        .iter()
        .filter(|install| install.new.is_some())
        .map(|install| install.path.as_str())
}

fn validate_selectors(selectors: &[String]) -> Result<()> {
    ensure!(!selectors.is_empty(), "restore selectors are empty");
    ensure!(
        selectors.len() <= MAX_SELECTORS,
        "restore exceeds selector count limit"
    );
    let mut total = 0usize;
    let mut previous: Option<&str> = None;
    for selector in selectors {
        validate_path(selector)?;
        total = total
            .checked_add(selector.len())
            .context("restore selector byte count overflow")?;
        ensure!(
            total <= MAX_TOTAL_PATH_BYTES,
            "restore selectors exceed path byte limit"
        );
        ensure!(
            previous.is_none_or(|prior| prior < selector.as_str()),
            "restore selectors are not canonical"
        );
        previous = Some(selector);
    }
    Ok(())
}

async fn expand_restore_selectors(
    repository: &LocalRepository,
    tree: TreeId,
    selectors: &[String],
) -> Result<std::collections::BTreeMap<String, FileValue>> {
    use wheat_store::TreeSelectionLimits;

    let files = MerkleTreeStore::new(&repository.objects)
        .select_paths(
            tree,
            selectors,
            TreeSelectionLimits {
                max_files: MAX_SELECTED_FILES,
                max_path_bytes: MAX_TOTAL_PATH_BYTES,
                max_selectors: MAX_SELECTORS,
                max_selector_path_bytes: MAX_TOTAL_PATH_BYTES,
                ..TreeSelectionLimits::default()
            },
        )
        .await?;
    Ok(files
        .into_iter()
        .map(|file| (file.path, file.value))
        .collect())
}

#[tracing::instrument(name = "recover_materialization", level = "debug", skip_all)]
pub async fn recover(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    *metadata = WorkspaceMetadata::read(root)?;
    recover_locked(root, repository, metadata).await
}

pub async fn recover_locked(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    let Some(journal) = read_journal(root)? else {
        cleanup_orphan_transactions(root)?;
        crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)?;
        return Ok(());
    };
    authenticate_journal(repository, metadata, &journal).await?;
    let (tx, mut progress) = open_transaction(root, journal.transaction_id)?;
    let progress_state = read_progress(
        &mut progress,
        journal.transaction_id,
        journal.installs.len(),
    )?;
    if progress_state.complete {
        return cleanup_committed(root, repository, &journal, &tx);
    }
    if progress_state.aborted {
        return cleanup_aborted(root, repository, &journal, &tx, progress_state.intents);
    }
    if journal.operation == Operation::Materialize
        && metadata.materialized.as_ref() == Some(&journal.target)
    {
        ensure!(
            progress_state.intents == journal.installs.len(),
            "published materialization is missing install intents"
        );
        append_completion(
            &mut progress,
            journal.transaction_id,
            journal.installs.len(),
        )?;
        checkpoint("completion-persisted")?;
        return cleanup_committed(root, repository, &journal, &tx);
    }
    ensure!(
        metadata.materialized == journal.source,
        "materialization journal conflicts with workspace metadata"
    );
    let planner = PlannerContext::new(&journal.installs);
    let complete = match roll_forward(root, repository, &tx, &mut progress, &journal, &planner) {
        Ok(complete) => complete,
        Err(error)
            if journal.operation == Operation::Restore
                && error.downcast_ref::<RestoreInvalidated>().is_some() =>
        {
            abort_restore(root, repository, &tx, &mut progress, &journal)?;
            return Err(error).context(
                "restore plan was invalidated by a changed selected path; preserved the new value",
            );
        }
        Err(error) => return Err(error),
    };
    if journal.operation == Operation::Materialize {
        publish_target(root, metadata, &journal)?;
        checkpoint("target-published")?;
    }
    if !complete {
        append_completion(
            &mut progress,
            journal.transaction_id,
            journal.installs.len(),
        )?;
        checkpoint("completion-persisted")?;
    }
    cleanup_committed(root, repository, &journal, &tx)?;
    crate::workspace_registry_service::reconcile(root, &metadata.store.path, metadata)
}

fn publish_target(root: &Path, metadata: &mut WorkspaceMetadata, journal: &Journal) -> Result<()> {
    let mut next = metadata.clone();
    next.active_label.clone_from(&journal.active_label);
    next.basis = journal.basis;
    next.observed_label_binding = journal.observed_label_binding;
    next.selected_frontier = journal.selected_frontier;
    next.materialized = Some(journal.target.clone());
    let prepared = next.replace_prepared(root)?;
    match prepared.publish() {
        Ok(()) => {
            *metadata = next;
            Ok(())
        }
        Err(WorkspaceMetadataPublishError::BeforePublish(error)) => {
            Err(error).context("publish workspace metadata")
        }
        Err(WorkspaceMetadataPublishError::PublishedButUnsynced(error)) => {
            *metadata = next;
            Err(error).context("workspace metadata published but directory sync failed")
        }
    }
}

async fn target_files(repository: &LocalRepository, tree: TreeId) -> Result<Vec<TreeFile>> {
    let files = MerkleTreeStore::new(&repository.objects)
        .walk_files(tree, TreeWalkLimits::default())
        .await?;
    let mut path_bytes = 0usize;
    for file in &files {
        validate_path(&file.path)?;
        path_bytes = path_bytes
            .checked_add(file.path.len())
            .context("tree path byte count overflow")?;
        ensure!(
            path_bytes <= MAX_TOTAL_PATH_BYTES,
            "tree exceeds aggregate path byte limit"
        );
    }
    Ok(files)
}

async fn plan_delta(
    repository: &LocalRepository,
    old: TreeId,
    new: TreeId,
    transaction_id: Uuid,
) -> Result<Vec<Install>> {
    let store = MerkleTreeStore::new(&repository.objects);
    let mut cursor = store.diff_cursor(old, new, TreeDiffLimits::default());
    let mut installs = Vec::new();
    let mut total = 0_u64;
    while let Some(change) = cursor.next().await? {
        validate_path(&change.path)?;
        if let Some(value) = change.new {
            total = total
                .checked_add(materializable_length(&repository.content, value)?)
                .context("materialization byte count overflow")?;
            ensure!(
                total <= MAX_TOTAL_BYTES,
                "materialization exceeds total byte limit"
            );
        }
        let index = installs.len();
        installs.push(Install {
            path: change.path,
            old: change.old,
            new: change.new,
            observed: None,
            restore_consent: false,
            empty_directory: false,
            staged: format!("leaf-{index:08x}"),
            tombstone: change
                .new
                .is_none()
                .then(|| format!("wheat-delete-v1:{transaction_id}:{index:08x}").into_bytes()),
        });
    }
    // Apply removals before additions so a tracked leaf may become a directory,
    // and remove deeper leaves first so an emptied tracked directory may become a leaf.
    installs.sort_by(|left, right| {
        let class = |install: &Install| match (install.old, install.new) {
            (Some(_), None) => 0_u8,
            (Some(_), Some(_)) => 1,
            (None, Some(_)) => 2,
            _ => 3,
        };
        class(left)
            .cmp(&class(right))
            .then_with(|| {
                let left_depth = left.path.matches('/').count();
                let right_depth = right.path.matches('/').count();
                if class(left) == 0 {
                    right_depth.cmp(&left_depth)
                } else {
                    left_depth.cmp(&right_depth)
                }
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    for (index, install) in installs.iter_mut().enumerate() {
        install.staged = format!("leaf-{index:08x}");
        install.tombstone = install
            .new
            .is_none()
            .then(|| format!("wheat-delete-v1:{transaction_id}:{index:08x}").into_bytes());
    }
    Ok(installs)
}

async fn authenticate_journal(
    repository: &LocalRepository,
    metadata: &WorkspaceMetadata,
    journal: &Journal,
) -> Result<()> {
    ensure!(
        journal.version == VERSION
            && journal.workspace_id == metadata.workspace_id
            && journal.store_id == metadata.store.store_id
            && journal.store_id == repository.store_id
            && (metadata.materialized == journal.source
                || metadata.materialized.as_ref() == Some(&journal.target)),
        "materialization journal does not belong to this workspace/store state"
    );
    let expected = match journal.operation {
        Operation::Materialize => {
            ensure!(
                journal.selectors.is_empty(),
                "materialization journal contains restore selectors"
            );
            authenticate_target(repository, journal).await?;
            match &journal.source {
                Some(source) => {
                    plan_delta(
                        repository,
                        source.tree,
                        journal.target.tree,
                        journal.transaction_id,
                    )
                    .await?
                }
                None => target_files(repository, journal.target.tree)
                    .await?
                    .into_iter()
                    .enumerate()
                    .map(|(index, file)| Install {
                        path: file.path,
                        old: None,
                        new: Some(file.value),
                        observed: None,
                        restore_consent: false,
                        empty_directory: false,
                        staged: format!("leaf-{index:08x}"),
                        tombstone: None,
                    })
                    .collect(),
            }
        }
        Operation::Restore => {
            ensure!(
                journal.source.as_ref() == Some(&journal.target)
                    && journal.active_label == metadata.active_label
                    && journal.basis == metadata.basis
                    && journal.observed_label_binding == metadata.observed_label_binding
                    && journal.selected_frontier == metadata.selected_frontier,
                "restore journal changes the pinned materialized state"
            );
            validate_selectors(&journal.selectors)?;
            let selected =
                expand_restore_selectors(repository, journal.target.tree, &journal.selectors)
                    .await?;
            ensure!(
                journal.installs.iter().all(|install| match install.new {
                    Some(value) => selected.get(&install.path).copied() == Some(value),
                    None => journal.selectors.iter().any(|selector| {
                        selector == &install.path
                            && selected.keys().any(|path| {
                                path.strip_prefix(selector)
                                    .is_some_and(|suffix| suffix.starts_with('/'))
                            })
                    }),
                }),
                "restore journal install is outside its canonical selectors"
            );
            ensure!(
                journal
                    .installs
                    .windows(2)
                    .all(|pair| pair[0].path < pair[1].path),
                "restore journal installs are not canonical"
            );
            journal
                .installs
                .iter()
                .enumerate()
                .map(|(index, install)| {
                    let valid_leaf = install.new.is_some()
                        && install.old.is_none_or(|old| Some(old) == install.new)
                        && (install.empty_directory
                            || install.old.is_some() == install.observed.is_some())
                        && !(install.empty_directory && install.observed.is_some())
                        && install.tombstone.is_none();
                    let valid_structure = install.new.is_none()
                        && install.old.is_some()
                        && install.observed.is_some()
                        && !install.empty_directory
                        && install.tombstone.is_some();
                    ensure!(
                        install.restore_consent
                            && (valid_leaf || valid_structure)
                            && install.staged == format!("leaf-{index:08x}"),
                        "invalid restore journal install"
                    );
                    Ok(install.clone())
                })
                .collect::<Result<Vec<_>>>()?
        }
    };
    ensure!(
        journal.installs == expected,
        "journal does not match authenticated tree delta"
    );
    Ok(())
}

async fn authenticate_target(repository: &LocalRepository, journal: &Journal) -> Result<()> {
    let head: RepositoryHead = journal.target.repository_head.into();
    let reader = wheat_store::RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let labels = reader.view_label_manifest(envelope.labels).await?;
    let pinned_binding = labels
        .labels
        .get(&journal.active_label)
        .map(|label| label.view)
        .with_context(|| {
            format!(
                "journal references unknown Wheat label {}",
                journal.active_label
            )
        })?;
    ensure!(
        pinned_binding == journal.observed_label_binding
            && journal.basis == journal.observed_label_binding,
        "journal label binding does not match its pinned basis"
    );
    let selected = journal.selected_frontier.unwrap_or(journal.basis);
    ensure!(
        selected == journal.target.view,
        "journal pinned frontier does not match its target view"
    );
    ensure!(
        reader.retained_view(&state, selected).await?.tree == journal.target.tree,
        "journal target view does not resolve to target tree"
    );
    Ok(())
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= MAX_PATH_BYTES
            && !path.starts_with('/')
            && !path.as_bytes().contains(&0),
        "invalid materialized path"
    );
    let mut components = path.split('/');
    let first = components.next().expect("nonempty");
    ensure!(
        !RESERVED.contains(&first),
        "tree contains reserved path {first:?}"
    );
    ensure!(
        first != "."
            && first != ".."
            && !first.is_empty()
            && first.len() <= 255
            && components.all(|part| {
                !part.is_empty() && part != "." && part != ".." && part.len() <= 255
            }),
        "invalid materialized path"
    );
    Ok(())
}

fn validate_materializable(repository: &LocalRepository, files: &[TreeFile]) -> Result<()> {
    let mut total = 0_u64;
    for file in files {
        total = total
            .checked_add(materializable_length(&repository.content, file.value)?)
            .context("materialization byte count overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES,
            "materialization exceeds total byte limit"
        );
    }
    Ok(())
}

/// Checks that `value` can be materialized and returns its byte length, from stored
/// length metadata so planning does not load every file. Only a symlink target, at most
/// 4,095 bytes, is read, to reject NUL bytes. Installation reads each file again and
/// enforces [`MAX_FILE_BYTES`] on the real bytes.
fn materializable_length(content: &ContentStore, value: FileValue) -> Result<u64> {
    let length = content.logical_length(value.content)?;
    ensure!(
        length <= MAX_FILE_BYTES as u64,
        "materialized file exceeds byte limit"
    );
    if value.kind == FileKind::Symlink {
        ensure!(
            (1..=4_095).contains(&length),
            "materialized symlink target is not representable"
        );
        let target = content.get_bounded(value.content, 4_095)?;
        ensure!(
            !target.contains(&0),
            "materialized symlink target is not representable"
        );
    }
    Ok(length)
}

fn validate_initial_workspace(root: &Path, files: &[TreeFile]) -> Result<()> {
    for file in files {
        match read_workspace_file(root, Path::new(&file.path), 0) {
            Ok(None) => {}
            Ok(Some(_)) => bail!(
                "refusing to overwrite untracked workspace path {:?}",
                file.path
            ),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect target path {:?}", file.path));
            }
        }
    }
    Ok(())
}

fn authenticate_install_preimage(
    root: &Path,
    install: &Install,
    expected_store_bytes: Option<&[u8]>,
) -> Result<bool> {
    if install.restore_consent {
        return match &install.observed {
            Some(observed) => match authenticate_workspace_preimage(
                root,
                Path::new(&install.path),
                observed,
                MAX_FILE_BYTES,
            ) {
                Ok(matches) => Ok(matches),
                Err(
                    WorkspaceFileError::IntermediateSymlink
                    | WorkspaceFileError::AncestorNotDirectory
                    | WorkspaceFileError::UnsupportedFileType
                    | WorkspaceFileError::TooLarge { .. },
                ) => Ok(false),
                Err(error) => Err(error.into()),
            },
            None => path_is_absent(root, &install.path),
        };
    }
    let expected = install
        .old
        .zip(expected_store_bytes)
        .map(|(value, bytes)| (value.kind, bytes));
    Ok(authenticate_workspace_file(
        root,
        Path::new(&install.path),
        expected,
        MAX_FILE_BYTES,
    )?)
}

/// Authenticate a materialization plan and drop the leaf operations the workspace
/// already satisfies.
///
/// A leaf that no longer holds its pinned value but already holds the exact target
/// value, or is already absent when the target deletes it, needs no installation.
/// Dropping it lets `materialize` proceed over paths that were written by other
/// means, such as captured workspace bytes, without overwriting anything. Every
/// other mismatch is refused exactly as before.
fn preflight_materialization(
    root: &Path,
    repository: &LocalRepository,
    installs: Vec<Install>,
) -> Result<Vec<Install>> {
    let satisfied = authenticate_preflight_with(
        root,
        repository,
        &installs,
        &PlannerContext::new(&installs),
        SatisfiedLeaves::Accept,
    )?;
    if satisfied.is_empty() {
        return Ok(installs);
    }
    let remaining = installs
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !satisfied.contains(index))
        .map(|(_, install)| install)
        .collect::<Vec<_>>();
    // Dropping operations can change the directory-replacement plan, so authenticate
    // the remaining plan exactly as it will be journaled.
    authenticate_preflight(
        root,
        repository,
        &remaining,
        &PlannerContext::new(&remaining),
    )?;
    Ok(remaining)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SatisfiedLeaves {
    Refuse,
    Accept,
}

fn authenticate_preflight(
    root: &Path,
    repository: &LocalRepository,
    installs: &[Install],
    planner: &PlannerContext,
) -> Result<()> {
    let satisfied =
        authenticate_preflight_with(root, repository, installs, planner, SatisfiedLeaves::Refuse)?;
    debug_assert!(satisfied.is_empty());
    Ok(())
}

/// Returns the indexes of plain leaf operations whose target the workspace already
/// holds. The set is always empty under [`SatisfiedLeaves::Refuse`].
fn authenticate_preflight_with(
    root: &Path,
    repository: &LocalRepository,
    installs: &[Install],
    planner: &PlannerContext,
    satisfied_leaves: SatisfiedLeaves,
) -> Result<BTreeSet<usize>> {
    let mut satisfied = BTreeSet::new();
    for (index, install) in installs.iter().enumerate() {
        if planner.is_shadowed_by_removed_leaf(&install.path) {
            continue;
        }
        if let Some(replacement) = planner.replaced_directories.get(install.path.as_str()) {
            if !path_is_directory(root, &install.path)? {
                // The planned replacement is already absent or a leaf.
            } else {
                authenticate_replaced_directory(
                    root,
                    &install.path,
                    replacement,
                    install.restore_consent,
                )?;
                continue;
            }
        }
        if install.empty_directory {
            let is_directory = path_is_directory(root, &install.path)?;
            let is_absent = !is_directory && path_is_absent(root, &install.path)?;
            if !is_absent && !(is_directory && directory_is_empty(root, &install.path)?) {
                return Err(restore_invalidated(format!(
                    "workspace directory {:?} changed from its exact observed structure",
                    install.path
                )));
            }
            continue;
        }
        let old_content = install
            .old
            .map(|value| repository.content.get(value.content))
            .transpose()?;
        let matches = authenticate_install_preimage(root, install, old_content.as_deref())
            .with_context(|| format!("preflight workspace path {:?}", install.path))?;
        if !matches && install.restore_consent {
            return Err(restore_invalidated(format!(
                "workspace path {:?} changed from its exact observed value",
                install.path
            )));
        }
        if !matches
            && satisfied_leaves == SatisfiedLeaves::Accept
            && workspace_holds(root, &repository.content, &install.path, install.new)
                .with_context(|| format!("preflight workspace path {:?}", install.path))?
        {
            satisfied.insert(index);
            continue;
        }
        ensure!(
            matches,
            "workspace path {:?} changed from its exact observed value",
            install.path
        );
    }
    Ok(satisfied)
}

fn roll_forward(
    root: &Path,
    repository: &LocalRepository,
    tx: &File,
    progress: &mut File,
    journal: &Journal,
    planner: &PlannerContext,
) -> Result<bool> {
    let state = read_progress(progress, journal.transaction_id, journal.installs.len())?;
    if state.complete {
        return Ok(true);
    }
    let completed = state.intents;
    if completed == 0 {
        // Re-authenticate the entire plan immediately before its first mutation.
        // This preserves all-or-nothing refusal when recovery starts after a user
        // edit, while the per-operation check below closes the remaining race.
        authenticate_preflight(root, repository, &journal.installs, planner)?;
    }
    // Directory-to-file transitions remove descendants before installing the leaf.
    // Re-inventory each such directory immediately before the first operation so a
    // concurrent untracked sibling is rejected before any tracked descendant moves.
    checkpoint("before-structural-recheck")?;
    for (directory, replacement) in &planner.replaced_directories {
        if completed <= replacement.first_descendant_operation {
            let restore_consent = journal.operation == Operation::Restore;
            authenticate_replaced_directory(root, directory, replacement, restore_consent)?;
        }
    }
    checkpoint("structural-recheck-complete")?;
    // A batch's intents are recorded together, and its renames become durable only
    // once the whole batch is applied, so any install of the last recorded batch
    // may have been interrupted. Settle each of them again, one at a time.
    let mut syncs = BatchSyncs::default();
    if state.batch_start < completed {
        let settled = (state.batch_start..completed).try_for_each(|index| {
            let loaded = load_install(repository, index, &journal.installs[index])?;
            #[cfg(test)]
            TEST_PEAK_OWNED_CONTENT_BYTES
                .fetch_max(loaded.resident_bytes as usize, Ordering::Relaxed);
            apply_loaded(root, tx, &loaded, planner, &mut syncs)?;
            checkpoint("installed")
        });
        syncs.flush(tx)?;
        settled?;
        checkpoint("batch-synced")?;
    }
    let mut batches = Batcher {
        repository,
        installs: &journal.installs,
        next: completed,
        carried: None,
    };
    loop {
        let mut batch = batches.next_batch()?;
        if batch.is_empty() {
            return Ok(false);
        }
        let (prepared, failure) = stage_batch(root, tx, &batch, planner)?;
        batch.truncate(prepared);
        let Some(last) = batch.last() else {
            return match failure {
                Some(error) => Err(error),
                None => Ok(false),
            };
        };
        let end = last.index + 1;
        checkpoint("staged")?;
        append_install_intents(progress, journal.transaction_id, end)?;
        checkpoint("intent-persisted")?;
        let applied = batch
            .iter()
            .try_for_each(|loaded| apply_batched(root, tx, loaded, planner, &mut syncs));
        // Renames already made stay durable even when a later install fails.
        syncs.flush(tx)?;
        applied?;
        checkpoint("batch-synced")?;
        if let Some(error) = failure {
            return Err(error);
        }
    }
}

fn apply_batched<'a>(
    root: &Path,
    tx: &File,
    loaded: &LoadedInstall<'a>,
    planner: &PlannerContext,
    syncs: &mut BatchSyncs<'a>,
) -> Result<()> {
    let install = loaded.install;
    if install.empty_directory {
        let (parent, name) = open_parent(root, &install.path)?;
        checkpoint("before-empty-directory-removal")?;
        match remove_directory_if_exists(&parent, name) {
            Ok(()) => {}
            Err(error) if is_directory_not_empty(&error) => {
                return Err(invalidated_install(
                    install,
                    format!(
                        "refusing to replace nonempty workspace directory {:?}",
                        install.path
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
        parent.sync_all()?;
        checkpoint("empty-directory-removed")?;
    }
    apply_loaded(root, tx, loaded, planner, syncs)?;
    checkpoint("installed")
}

/// An install with its content, read from the store once for its whole batch.
struct LoadedInstall<'a> {
    index: usize,
    install: &'a Install,
    old: Option<Vec<u8>>,
    /// `None` when the new content is the old content; see `new_bytes`.
    new: Option<Vec<u8>>,
    shared: bool,
    resident_bytes: u64,
}

impl LoadedInstall<'_> {
    fn old_bytes(&self) -> Option<&[u8]> {
        self.old.as_deref()
    }

    fn new_bytes(&self) -> Option<&[u8]> {
        if self.shared {
            self.old.as_deref()
        } else {
            self.new.as_deref()
        }
    }
}

fn load_install<'a>(
    repository: &LocalRepository,
    index: usize,
    install: &'a Install,
) -> Result<LoadedInstall<'a>> {
    // If old and new have the same content ID, retain one copy and share it
    // between both descriptors.
    let old = install
        .old
        .map(|value| repository.content.get(value.content))
        .transpose()?;
    let shared = install
        .old
        .zip(install.new)
        .is_some_and(|(old, new)| old.content == new.content);
    let new = if shared {
        None
    } else {
        install
            .new
            .map(|value| repository.content.get(value.content))
            .transpose()?
    };
    for bytes in [old.as_ref(), new.as_ref()].into_iter().flatten() {
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "materialized file exceeds byte limit"
        );
    }
    let resident_bytes = old
        .as_ref()
        .map_or(0_u64, |bytes| bytes.len() as u64)
        .checked_add(new.as_ref().map_or(0_u64, |bytes| bytes.len() as u64))
        .context("materialization resident byte count overflow")?;
    ensure!(
        resident_bytes <= MAX_OPERATION_RESIDENT_BYTES,
        "materialization operation exceeds resident byte limit"
    );
    Ok(LoadedInstall {
        index,
        install,
        old,
        new,
        shared,
        resident_bytes,
    })
}

/// Groups the unrecorded installs into batches. Installs in one batch must not
/// depend on each other being applied first: no install's path is an ancestor of
/// another's (a deleted file whose path becomes a directory, or a directory emptied
/// to become a file). An empty-directory restore is alone in its batch, since its
/// directory is removed after its intent.
struct Batcher<'a> {
    repository: &'a LocalRepository,
    installs: &'a [Install],
    next: usize,
    /// An install loaded for a batch its content did not fit, which starts the
    /// next batch.
    carried: Option<LoadedInstall<'a>>,
}

impl<'a> Batcher<'a> {
    /// The next batch, empty once every install is batched.
    fn next_batch(&mut self) -> Result<Vec<LoadedInstall<'a>>> {
        let mut batch = Vec::<LoadedInstall<'a>>::new();
        let mut paths = HashSet::<&str>::new();
        let mut ancestors = HashSet::<&str>::new();
        let mut resident = 0_u64;
        while batch.len() < MAX_BATCH_INSTALLS {
            let index = self.next + batch.len();
            let Some(install) = self.installs.get(index) else {
                break;
            };
            if let Some(first) = batch.first()
                && (install.empty_directory
                    || first.install.empty_directory
                    || ancestors.contains(install.path.as_str())
                    || path_ancestors(&install.path).any(|ancestor| paths.contains(ancestor)))
            {
                break;
            }
            let loaded = match self.carried.take() {
                Some(loaded) => loaded,
                None => load_install(self.repository, index, install)?,
            };
            debug_assert_eq!(loaded.index, index);
            if !batch.is_empty() && resident + loaded.resident_bytes > MAX_BATCH_RESIDENT_BYTES {
                self.carried = Some(loaded);
                break;
            }
            resident += loaded.resident_bytes;
            paths.insert(install.path.as_str());
            ancestors.extend(path_ancestors(&install.path));
            batch.push(loaded);
        }
        #[cfg(test)]
        TEST_PEAK_OWNED_CONTENT_BYTES.fetch_max(resident as usize, Ordering::Relaxed);
        self.next += batch.len();
        Ok(batch)
    }
}

/// The proper ancestors of a relative path, nearest first.
fn path_ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(path.rsplit_once('/').map(|(parent, _)| parent), |path| {
        path.rsplit_once('/').map(|(parent, _)| parent)
    })
}

/// Workspace directories whose renames a batch has not yet made durable. Each is
/// synced once when the batch is applied, then the transaction directory.
#[derive(Default)]
struct BatchSyncs<'a> {
    directories: HashMap<&'a str, File>,
}

impl<'a> BatchSyncs<'a> {
    /// Remembers `parent`, the open parent directory of `path`.
    fn defer(&mut self, path: &'a str, parent: File) {
        let key = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        self.directories.entry(key).or_insert(parent);
    }

    #[tracing::instrument(name = "materialization_batch_sync", level = "debug", skip_all)]
    fn flush(&mut self, tx: &File) -> Result<()> {
        for (_, directory) in self.directories.drain() {
            directory.sync_all()?;
        }
        tx.sync_all()?;
        Ok(())
    }
}

fn apply_loaded<'a>(
    root: &Path,
    tx: &File,
    loaded: &LoadedInstall<'a>,
    planner: &PlannerContext,
    syncs: &mut BatchSyncs<'a>,
) -> Result<()> {
    apply_or_recover_install(
        root,
        tx,
        loaded.install,
        loaded.old_bytes(),
        loaded.new_bytes(),
        planner,
        syncs,
    )
}

/// A leaf to write into the transaction directory.
struct StagedLeaf<'a> {
    name: &'a str,
    kind: FileKind,
    bytes: &'a [u8],
}

/// Checks and prepares a batch's installs in order, writes the staged leaves that
/// are missing, and makes them durable with one sync of the transaction directory.
///
/// An install that fails its check ends the batch there: the installs before it
/// are still staged, and the failure is returned with their count so they can be
/// recorded and applied first, as when installs went one at a time.
#[tracing::instrument(name = "materialization_stage_batch", level = "debug", skip_all)]
fn stage_batch(
    root: &Path,
    tx: &File,
    batch: &[LoadedInstall<'_>],
    planner: &PlannerContext,
) -> Result<(usize, Option<anyhow::Error>)> {
    let mut leaves = Vec::new();
    let mut prepared = 0;
    let mut failure = None;
    let prepare = tracing::debug_span!("materialization_prepare").entered();
    for loaded in batch {
        match prepare_install(root, tx, loaded, planner) {
            Ok(leaf) => leaves.extend(leaf),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
        prepared += 1;
    }
    drop(prepare);
    if !leaves.is_empty() {
        write_staged_leaves(tx, &leaves)?;
        tx.sync_all()?;
    }
    Ok((prepared, failure))
}

/// Where a staged leaf is written before it is synced and renamed to its staged
/// name. A staged name therefore only ever holds a complete, synced leaf, and a
/// crash while staging leaves writing debris that never held workspace data.
fn writing_name(staged: &str) -> String {
    format!("writing-{staged}")
}

/// Writes `leaves` on up to `STAGING_WORKERS` threads, so the filesystem can
/// commit their syncs together.
#[tracing::instrument(name = "materialization_write_leaves", level = "debug", skip_all)]
fn write_staged_leaves(tx: &File, leaves: &[StagedLeaf<'_>]) -> Result<()> {
    let next = AtomicUsize::new(0);
    let write = || -> Result<()> {
        while let Some(leaf) = leaves.get(next.fetch_add(1, Ordering::Relaxed)) {
            let writing = writing_name(leaf.name);
            create_leaf_at(tx, &writing, leaf.kind, leaf.bytes)?;
            rename_noreplace(tx, &writing, tx, leaf.name)?;
        }
        Ok(())
    };
    let helpers = STAGING_WORKERS.min(leaves.len()).saturating_sub(1);
    std::thread::scope(|scope| {
        let handles = (0..helpers).map(|_| scope.spawn(write)).collect::<Vec<_>>();
        let mut result = write();
        for handle in handles {
            let helper = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            result = result.and(helper);
        }
        result
    })
}

/// Checks that the workspace still holds the install's old value and prepares its
/// parents. Returns the staged leaf to write, or `None` when an earlier run already
/// staged it.
fn prepare_install<'a>(
    root: &Path,
    tx: &File,
    loaded: &'a LoadedInstall<'a>,
    planner: &PlannerContext,
) -> Result<Option<StagedLeaf<'a>>> {
    let install = loaded.install;
    let old_bytes = loaded.old_bytes();
    let new_bytes = loaded.new_bytes();
    if !install.empty_directory {
        remove_replaced_directory(root, tx, install, planner)?;
    }
    if install.empty_directory {
        if !(path_is_directory(root, &install.path)? && directory_is_empty(root, &install.path)?) {
            return Err(restore_invalidated(format!(
                "workspace directory {:?} changed after consent",
                install.path
            )));
        }
    } else if !authenticate_install_preimage(root, install, old_bytes)? {
        if install.restore_consent {
            return Err(restore_invalidated(format!(
                "workspace path {:?} changed after preflight",
                install.path
            )));
        }
        bail!("workspace path {:?} changed after preflight", install.path);
    }
    let leaf = match (install.new, new_bytes, install.tombstone.as_deref()) {
        (Some(value), Some(bytes), None) => StagedLeaf {
            name: &install.staged,
            kind: value.kind,
            bytes,
        },
        (None, None, Some(marker)) => StagedLeaf {
            name: &install.staged,
            kind: FileKind::Regular,
            bytes: marker,
        },
        _ => bail!("invalid materialization operation"),
    };
    let missing = if leaf_exists(tx, leaf.name)? {
        ensure!(
            authenticate_leaf_at_bytes(tx, leaf.name, leaf.kind, leaf.bytes)?,
            "staged transaction leaf changed before intent"
        );
        None
    } else {
        // Debris from writing this leaf when a crash stopped an earlier run.
        remove_file_if_exists(tx, &writing_name(leaf.name))?;
        Some(leaf)
    };
    ensure_parent_directories(root, &install.path)?;
    Ok(missing)
}

fn apply_or_recover_install<'a>(
    root: &Path,
    tx: &File,
    install: &'a Install,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    planner: &PlannerContext,
    syncs: &mut BatchSyncs<'a>,
) -> Result<()> {
    match (install.old, install.new) {
        (None, Some(new)) => {
            let (parent, name) = open_parent(root, &install.path)?;
            let new_bytes = new_bytes.context("creation target content is unavailable")?;
            if leaf_exists(tx, &install.staged)? {
                if install.empty_directory {
                    if path_is_directory(root, &install.path)? {
                        if !directory_is_empty(root, &install.path)? {
                            return Err(invalidated_install(
                                install,
                                format!(
                                    "workspace directory {:?} changed after restore intent",
                                    install.path
                                ),
                            ));
                        }
                        match remove_directory_if_exists(&parent, name) {
                            Ok(()) => parent.sync_all()?,
                            Err(error) if is_directory_not_empty(&error) => {
                                return Err(invalidated_install(
                                    install,
                                    format!(
                                        "workspace directory {:?} changed after restore intent",
                                        install.path
                                    ),
                                ));
                            }
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!("remove observed empty directory {:?}", install.path)
                                });
                            }
                        }
                    } else if !path_is_absent(root, &install.path)? {
                        return Err(invalidated_install(
                            install,
                            format!(
                                "workspace directory {:?} changed after restore intent",
                                install.path
                            ),
                        ));
                    }
                } else if !path_is_absent(root, &install.path)? {
                    return Err(invalidated_install(
                        install,
                        format!("workspace collision while creating {:?}", install.path),
                    ));
                }
                authenticate_exact_at_bytes(tx, &install.staged, new, new_bytes)?;
                rename_noreplace(tx, &install.staged, &parent, name)?;
            }
            // A rename an earlier run made in this boot may not be durable yet.
            syncs.defer(&install.path, parent);
            ensure!(
                authenticate_workspace_file(
                    root,
                    Path::new(&install.path),
                    Some((new.kind, new_bytes)),
                    MAX_FILE_BYTES,
                )?,
                "created workspace path is missing or changed"
            );
        }
        (Some(_old), Some(new)) => {
            let (parent, name) = open_parent(root, &install.path)?;
            let old_bytes = old_bytes.context("replacement preimage content is unavailable")?;
            let new_bytes = new_bytes.context("replacement target content is unavailable")?;
            let workspace_is_old = authenticate_install_preimage(root, install, Some(old_bytes))?;
            let staged_is_new =
                authenticate_leaf_at_bytes(tx, &install.staged, new.kind, new_bytes)?;
            if workspace_is_old && staged_is_new {
                checkpoint("before-exchange")?;
                rename_exchange(tx, &install.staged, &parent, name)?;
                checkpoint("exchanged")?;
                if !authenticate_displaced_at(tx, &install.staged, install, old_bytes)? {
                    if authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((new.kind, new_bytes)),
                        MAX_FILE_BYTES,
                    )? {
                        rename_exchange(tx, &install.staged, &parent, name)?;
                        parent.sync_all()?;
                        tx.sync_all()?;
                        return Err(invalidated_install(
                            install,
                            format!(
                                "workspace path {:?} changed during replacement; restored concurrent data",
                                install.path
                            ),
                        ));
                    }
                    parent.sync_all()?;
                    tx.sync_all()?;
                    return Err(invalidated_install(
                        install,
                        format!(
                            "workspace path {:?} changed during replacement; preserved both values",
                            install.path
                        ),
                    ));
                }
            } else {
                if staged_is_new {
                    return Err(invalidated_install(
                        install,
                        format!(
                            "workspace path {:?} changed after replacement intent",
                            install.path
                        ),
                    ));
                }
                ensure!(
                    authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((new.kind, new_bytes)),
                        MAX_FILE_BYTES
                    )? && authenticate_displaced_at(tx, &install.staged, install, old_bytes)?,
                    "ambiguous interrupted replacement for {:?}",
                    install.path
                );
            }
            // An exchange an earlier run made in this boot may not be durable yet.
            syncs.defer(&install.path, parent);
        }
        (Some(_old), None) => {
            // A completed descendant deletion may already have removed its empty
            // plan-owned parent before a crash. Recreate that directory only as
            // transaction scaffolding; the later directory-to-file install removes
            // it after authenticating the displaced leaf.
            ensure_parent_directories(root, &install.path)?;
            let (parent, name) = open_parent(root, &install.path)?;
            let old_bytes = old_bytes.context("deletion preimage content is unavailable")?;
            let marker = install
                .tombstone
                .as_deref()
                .context("deletion has no tombstone")?;
            let workspace_is_old = authenticate_install_preimage(root, install, Some(old_bytes))?;
            let staged_is_marker =
                authenticate_leaf_at_bytes(tx, &install.staged, FileKind::Regular, marker)?;
            if workspace_is_old && staged_is_marker {
                checkpoint("before-exchange")?;
                rename_exchange(tx, &install.staged, &parent, name)?;
                parent.sync_all()?;
                tx.sync_all()?;
                checkpoint("exchanged")?;
                if !authenticate_displaced_at(tx, &install.staged, install, old_bytes)? {
                    if authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((FileKind::Regular, marker)),
                        MAX_FILE_BYTES,
                    )? {
                        rename_exchange(tx, &install.staged, &parent, name)?;
                        parent.sync_all()?;
                        tx.sync_all()?;
                        return Err(invalidated_install(
                            install,
                            format!(
                                "workspace path {:?} changed during deletion exchange; restored concurrent data",
                                install.path
                            ),
                        ));
                    }
                    return Err(invalidated_install(
                        install,
                        format!(
                            "workspace path {:?} changed during deletion exchange; preserved both values",
                            install.path
                        ),
                    ));
                }
            } else {
                if staged_is_marker {
                    return Err(invalidated_install(
                        install,
                        format!(
                            "workspace path {:?} changed after deletion intent",
                            install.path
                        ),
                    ));
                }
                ensure!(
                    authenticate_displaced_at(tx, &install.staged, install, old_bytes)?
                        && (authenticate_workspace_file(
                            root,
                            Path::new(&install.path),
                            None,
                            MAX_FILE_BYTES
                        )? || authenticate_workspace_file(
                            root,
                            Path::new(&install.path),
                            Some((FileKind::Regular, marker)),
                            MAX_FILE_BYTES
                        )?),
                    "ambiguous interrupted deletion for {:?}",
                    install.path
                );
            }
            let removed = install.staged.replacen("leaf-", "removed-", 1);
            if !leaf_exists(tx, &removed)?
                && !authenticate_workspace_file(
                    root,
                    Path::new(&install.path),
                    None,
                    MAX_FILE_BYTES,
                )?
            {
                checkpoint("before-deletion-move")?;
                rename_noreplace(&parent, name, tx, &removed)?;
                parent.sync_all()?;
                tx.sync_all()?;
                checkpoint("deletion-unlinked")?;
            }
            if leaf_exists(tx, &removed)? {
                if !authenticate_leaf_at_bytes(tx, &removed, FileKind::Regular, marker)? {
                    if read_workspace_file(root, Path::new(&install.path), 0)?.is_none() {
                        rename_noreplace(tx, &removed, &parent, name)?;
                        parent.sync_all()?;
                        tx.sync_all()?;
                        return Err(invalidated_install(
                            install,
                            format!(
                                "workspace path {:?} changed during deletion move; restored concurrent data",
                                install.path
                            ),
                        ));
                    }
                    return Err(invalidated_install(
                        install,
                        format!(
                            "workspace path {:?} changed during deletion move; preserved concurrent data",
                            install.path
                        ),
                    ));
                }
                remove_file_if_exists(tx, &removed)?;
                tx.sync_all()?;
            }
            remove_empty_owned_parents(root, &install.path, planner)?;
        }
        _ => bail!("invalid materialization operation"),
    }
    Ok(())
}

fn authenticate_displaced_at(
    directory: &File,
    name: &str,
    install: &Install,
    old_bytes: &[u8],
) -> Result<bool> {
    if install.restore_consent {
        let Some(observed) = &install.observed else {
            return Ok(false);
        };
        return Ok(authenticate_preimage_at(
            directory,
            OsStr::new(name),
            observed,
            MAX_FILE_BYTES,
        )?);
    }
    let old = install.old.context("install has no old value")?;
    authenticate_leaf_at_bytes(directory, name, old.kind, old_bytes)
}

fn authenticate_snapshot_bytes(
    value: FileValue,
    expected: &[u8],
    actual: &WorkspaceFileSnapshot,
) -> Result<()> {
    ensure!(
        actual.kind == value.kind && actual.bytes == expected,
        "materialized target changed externally"
    );
    Ok(())
}

fn authenticate_exact_at_bytes(
    directory: &File,
    name: &str,
    value: FileValue,
    bytes: &[u8],
) -> Result<()> {
    let actual = read_leaf_at(directory, name, MAX_FILE_BYTES)?;
    authenticate_snapshot_bytes(value, bytes, &actual)
}

fn create_transaction(root: &Path, id: Uuid) -> Result<(File, File)> {
    let control = open_directory(&root.join(".wheat"))?;
    mkdirat_if_missing(&control, TRANSACTIONS, 0o700)?;
    let transactions = open_dir_at(&control, TRANSACTIONS)?;
    let name = id.to_string();
    mkdirat_new(&transactions, &name, 0o700)?;
    transactions.sync_all()?;
    checkpoint("transaction-directory-created")?;
    let tx = open_dir_at(&transactions, &name)?;
    let progress = create_progress(&tx)?;
    tx.sync_all()?;
    checkpoint("progress-created")?;
    Ok((tx, progress))
}

fn open_transaction(root: &Path, id: Uuid) -> Result<(File, File)> {
    let control = open_directory(&root.join(".wheat"))?;
    let transactions = open_dir_at(&control, TRANSACTIONS)?;
    let tx = open_dir_at(&transactions, &id.to_string())?;
    let progress = open_progress(&tx)?;
    Ok((tx, progress))
}

fn create_progress(tx: &File) -> Result<File> {
    open_progress_with(tx, sys::Creation::New)
}

fn open_progress(tx: &File) -> Result<File> {
    open_progress_with(tx, sys::Creation::Existing)
}

fn open_progress_with(tx: &File, creation: sys::Creation) -> Result<File> {
    let file = sys::open_file_at(
        tx,
        PROGRESS_NAME,
        sys::OpenFile {
            read: true,
            write: true,
            append: true,
            creation,
            mode: 0o600,
        },
    )
    .context("open materialization progress log")?;
    ensure!(
        file.metadata()?.is_file(),
        "materialization progress log is not a regular file"
    );
    Ok(file)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProgressState {
    /// Installs with a recorded intent, always a prefix of the plan.
    intents: usize,
    /// The first install of the last recorded batch.
    batch_start: usize,
    complete: bool,
    aborted: bool,
}

/// Records intents for a batch: every install before `end`. One record per batch
/// means a crash can only tear the last record.
fn append_install_intents(progress: &mut File, transaction_id: Uuid, end: usize) -> Result<()> {
    append_progress_record(
        progress,
        transaction_id,
        PROGRESS_OPCODE_INSTALL_INTENT,
        end,
    )
}

fn append_completion(progress: &mut File, transaction_id: Uuid, installs: usize) -> Result<()> {
    append_progress_record(progress, transaction_id, PROGRESS_OPCODE_COMPLETE, installs)
}

fn append_aborted(progress: &mut File, transaction_id: Uuid, intents: usize) -> Result<()> {
    append_progress_record(progress, transaction_id, PROGRESS_OPCODE_ABORTED, intents)
}

fn append_progress_record(
    progress: &mut File,
    transaction_id: Uuid,
    opcode: u16,
    index: usize,
) -> Result<()> {
    let fields = MaterializationProgressFields {
        opcode,
        transaction: *transaction_id.as_bytes(),
        index: u64::try_from(index)?,
    };
    let checksum = crc32fast::hash(&encode_materialization_progress_fields(&fields)?);
    let record = encode_materialization_progress_record(&fields, checksum)?;
    debug_assert_eq!(record.len(), PROGRESS_RECORD_BYTES);
    progress.write_all(&record)?;
    progress.sync_data()?;
    Ok(())
}

fn read_progress(
    progress: &mut File,
    transaction_id: Uuid,
    installs: usize,
) -> Result<ProgressState> {
    let length = usize::try_from(progress.metadata()?.len())?;
    let complete_bytes = length / PROGRESS_RECORD_BYTES * PROGRESS_RECORD_BYTES;
    // One extra full-width tail beyond the maximum valid records may be a torn
    // append and is parsed below so it receives the same final-record truncation.
    ensure!(
        complete_bytes / PROGRESS_RECORD_BYTES <= installs.saturating_add(2),
        "materialization progress exceeds install count"
    );
    progress.seek(SeekFrom::Start(0))?;
    let mut record = [0_u8; PROGRESS_RECORD_BYTES];
    let mut valid = 0usize;
    let mut state = ProgressState {
        intents: 0,
        batch_start: 0,
        complete: false,
        aborted: false,
    };
    let complete_records = complete_bytes / PROGRESS_RECORD_BYTES;
    for physical_index in 0..complete_records {
        progress.read_exact(&mut record)?;
        let fields = match validate_progress_record(&record, transaction_id, installs, state) {
            Ok(fields) => fields,
            // An interrupted append may leave a full-width record whose payload or
            // checksum never reached stable storage. Only the last physical record
            // can be such a torn append; corruption before it is unambiguously in
            // committed progress and remains a hard error.
            Err(_) if physical_index + 1 == complete_records => break,
            Err(error) => {
                return Err(error)
                    .context("materialization progress contains an invalid complete record");
            }
        };
        match fields.opcode {
            PROGRESS_OPCODE_INSTALL_INTENT => {
                state.batch_start = state.intents;
                state.intents = usize::try_from(fields.index)?;
            }
            PROGRESS_OPCODE_COMPLETE => state.complete = true,
            PROGRESS_OPCODE_ABORTED => state.aborted = true,
            _ => unreachable!("validated progress opcode"),
        }
        valid += 1;
    }
    let valid_bytes = valid * PROGRESS_RECORD_BYTES;
    if valid_bytes != length {
        progress.set_len(u64::try_from(valid_bytes)?)?;
        progress.sync_data()?;
    }
    progress.seek(SeekFrom::End(0))?;
    Ok(state)
}

fn validate_progress_record(
    record: &[u8; PROGRESS_RECORD_BYTES],
    id: Uuid,
    installs: usize,
    state: ProgressState,
) -> Result<MaterializationProgressFields> {
    let fields = decode_progress_record(record)?;
    ensure!(
        &fields.transaction == id.as_bytes(),
        "progress transaction mismatch"
    );
    let index = usize::try_from(fields.index)?;
    match fields.opcode {
        PROGRESS_OPCODE_INSTALL_INTENT => {
            ensure!(
                !state.complete && !state.aborted,
                "install intent follows terminal record"
            );
            ensure!(
                index > state.intents,
                "non-contiguous materialization progress"
            );
            ensure!(index <= installs, "too many install intents");
            ensure!(
                index - state.intents <= MAX_BATCH_INSTALLS,
                "materialization batch exceeds install limit"
            );
        }
        PROGRESS_OPCODE_COMPLETE => {
            ensure!(
                !state.complete && !state.aborted,
                "duplicate terminal record"
            );
            ensure!(
                state.intents == installs && index == installs,
                "completion record precedes all install intents"
            );
        }
        PROGRESS_OPCODE_ABORTED => {
            ensure!(
                !state.complete && !state.aborted,
                "duplicate terminal record"
            );
            ensure!(
                state.intents < installs && index == state.intents,
                "invalid aborted record"
            );
        }
        _ => bail!("invalid progress opcode"),
    }
    Ok(fields)
}

/// Decodes a progress record whose checksum matches its fields. Its transaction
/// and its place in the log are the caller's to check.
fn decode_progress_record(
    record: &[u8; PROGRESS_RECORD_BYTES],
) -> Result<MaterializationProgressFields> {
    let (fields, checksum) =
        decode_materialization_progress_record(record).context("invalid progress record")?;
    ensure!(
        crc32fast::hash(&encode_materialization_progress_fields(&fields)?) == checksum,
        "invalid progress checksum"
    );
    Ok(fields)
}

fn journal_exists(root: &Path) -> Result<bool> {
    let control = open_directory(&root.join(".wheat"))?;
    leaf_exists(&control, JOURNAL_NAME)
}

fn read_journal(root: &Path) -> Result<Option<Journal>> {
    let control = open_directory(&root.join(".wheat"))?;
    let file = match sys::open_file_at(&control, JOURNAL_NAME, sys::OpenFile::READ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context("open materialization journal without following symlinks");
        }
    };
    ensure!(
        file.metadata()?.is_file(),
        "materialization journal is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_JOURNAL_BYTES)?.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_JOURNAL_BYTES,
        "materialization journal exceeds byte limit"
    );
    let journal = serde_json::from_slice(&bytes).context("decode materialization journal")?;
    validate_journal_bounds(&journal)?;
    Ok(Some(journal))
}

fn validate_journal_bounds(journal: &Journal) -> Result<()> {
    ensure!(
        journal.installs.len() <= MAX_SELECTED_FILES,
        "materialization journal exceeds install count"
    );
    let mut path_bytes = 0usize;
    for install in &journal.installs {
        validate_path(&install.path)?;
        ensure!(
            install.staged.len() <= 64,
            "journal staged name exceeds byte limit"
        );
        ensure!(
            install
                .tombstone
                .as_ref()
                .is_none_or(|marker| marker.len() <= 128),
            "journal tombstone exceeds byte limit"
        );
        path_bytes = path_bytes
            .checked_add(install.path.len())
            .context("journal path byte count overflow")?;
        ensure!(
            path_bytes <= MAX_TOTAL_PATH_BYTES,
            "journal exceeds path byte limit"
        );
    }
    match journal.operation {
        Operation::Materialize => {
            ensure!(
                journal.selectors.is_empty(),
                "materialization journal contains restore selectors"
            );
            ensure!(
                journal
                    .installs
                    .iter()
                    .all(|install| !install.empty_directory),
                "materialization journal contains restore structure"
            );
        }
        Operation::Restore => {
            validate_selectors(&journal.selectors)?;
            ensure!(
                !journal.installs.is_empty(),
                "restore journal contains no changed installs"
            );
        }
    }
    Ok(())
}

struct BoundedJournalWriter {
    bytes: Vec<u8>,
}

impl Write for BoundedJournalWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = (MAX_JOURNAL_BYTES - 1).saturating_sub(self.bytes.len());
        if buffer.len() > remaining {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "materialization journal exceeds byte limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_journal(journal: &Journal) -> Result<Vec<u8>> {
    validate_journal_bounds(journal)?;
    let mut writer = BoundedJournalWriter {
        bytes: Vec::with_capacity(MAX_JOURNAL_BYTES.min(64 * 1024)),
    };
    serde_json::to_writer(&mut writer, journal)
        .context("encode bounded materialization journal")?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

fn write_journal(root: &Path, bytes: &[u8]) -> Result<()> {
    let control_path = root.join(".wheat");
    let control = open_directory(&control_path)?;
    let temporary_name = format!(".materialization-{}.tmp", Uuid::now_v7());
    let mut temporary = create_regular_at(&control, &temporary_name, 0o600)?;
    let result = (|| {
        temporary.write_all(bytes)?;
        temporary.sync_all()?;
        rename_noreplace(&control, &temporary_name, &control, JOURNAL_NAME)?;
        control.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_file_if_exists(&control, &temporary_name);
    }
    result
}

fn abort_restore(
    root: &Path,
    repository: &LocalRepository,
    tx: &File,
    progress: &mut File,
    journal: &Journal,
) -> Result<()> {
    ensure!(
        journal.operation == Operation::Restore,
        "cannot abort materialization"
    );
    let state = read_progress(progress, journal.transaction_id, journal.installs.len())?;
    if !state.aborted {
        append_aborted(progress, journal.transaction_id, state.intents)?;
        checkpoint("abort-persisted")?;
    }
    cleanup_aborted(root, repository, journal, tx, state.intents)
}

fn cleanup_aborted(
    root: &Path,
    repository: &LocalRepository,
    journal: &Journal,
    tx: &File,
    intents: usize,
) -> Result<()> {
    // A batch is staged in full before its intents are appended. Consequently the
    // intended prefix may have apply debris, and the batch after it (at most
    // `MAX_BATCH_INSTALLS` operations) may have authenticated staged targets and
    // writing debris. Later operation names are never authorized by an abort record.
    for (index, install) in journal
        .installs
        .iter()
        .take(intents.saturating_add(MAX_BATCH_INSTALLS))
        .enumerate()
    {
        if index >= intents {
            remove_file_if_exists(tx, &writing_name(&install.staged))?;
        }
        if leaf_exists(tx, &install.staged)? {
            let is_staged_target = match install.new {
                Some(target) => {
                    let target_bytes = repository.content.get(target.content)?;
                    authenticate_leaf_at_bytes(tx, &install.staged, target.kind, &target_bytes)?
                }
                None => false,
            };
            let is_tombstone = match install.tombstone.as_deref() {
                Some(marker) => {
                    authenticate_leaf_at_bytes(tx, &install.staged, FileKind::Regular, marker)?
                }
                None => false,
            };
            let is_displaced_preimage = if index < intents {
                match &install.observed {
                    Some(observed) => authenticate_preimage_at(
                        tx,
                        OsStr::new(&install.staged),
                        observed,
                        MAX_FILE_BYTES,
                    )?,
                    None => false,
                }
            } else {
                false
            };
            ensure!(
                is_staged_target || is_tombstone || is_displaced_preimage,
                "refusing to remove unauthenticated aborted restore debris for {:?}",
                install.path
            );
            remove_file_if_exists(tx, &install.staged)?;
            tx.sync_all()?;
            checkpoint("aborted-preimage-unlinked")?;
        }
        if index >= intents {
            continue;
        }
        let removed = install.staged.replacen("leaf-", "removed-", 1);
        if leaf_exists(tx, &removed)? {
            let marker = install
                .tombstone
                .as_deref()
                .context("aborted deletion debris has no tombstone")?;
            ensure!(
                authenticate_leaf_at_bytes(tx, &removed, FileKind::Regular, marker)?,
                "refusing to remove unauthenticated aborted deletion debris for {:?}",
                install.path
            );
            remove_file_if_exists(tx, &removed)?;
            tx.sync_all()?;
            checkpoint("aborted-removed-unlinked")?;
        }
    }
    ensure_progress_only(tx)?;
    let control = open_directory(&root.join(".wheat"))?;
    remove_file_if_exists(&control, JOURNAL_NAME)?;
    control.sync_all()?;
    checkpoint("aborted-journal-unlinked")?;
    cleanup_abandoned_transaction(root, journal.transaction_id, tx)
}

/// Remove authenticated displaced leaves only after durable commit (the progress
/// completion record), then remove the journal that makes the transaction
/// recoverable.
fn cleanup_committed(
    root: &Path,
    repository: &LocalRepository,
    journal: &Journal,
    tx: &File,
) -> Result<()> {
    let mut progress = open_progress(tx)?;
    ensure!(
        read_progress(
            &mut progress,
            journal.transaction_id,
            journal.installs.len()
        )?
        .complete,
        "refusing cleanup before durable completion"
    );
    for install in &journal.installs {
        match install.old {
            Some(old) if leaf_exists(tx, &install.staged)? => {
                let old_bytes = repository.content.get(old.content)?;
                ensure!(
                    authenticate_displaced_at(tx, &install.staged, install, &old_bytes)?,
                    "displaced preimage for {:?} changed",
                    install.path
                );
                remove_file_if_exists(tx, &install.staged)?;
                checkpoint("staged-preimage-unlinked")?;
            }
            _ => {}
        }
    }
    tx.sync_all()?;
    ensure_progress_only(tx)?;
    let control = open_directory(&root.join(".wheat"))?;
    remove_file_if_exists(&control, JOURNAL_NAME)?;
    checkpoint("journal-unlinked")?;
    control.sync_all()?;
    cleanup_abandoned_transaction(root, journal.transaction_id, tx)
}

fn cleanup_abandoned_transaction(root: &Path, transaction_id: Uuid, tx: &File) -> Result<()> {
    remove_file_if_exists(tx, PROGRESS_NAME)?;
    tx.sync_all()?;
    let control = open_directory(&root.join(".wheat"))?;
    let transactions = open_dir_at(&control, TRANSACTIONS)?;
    remove_directory_if_exists(&transactions, &transaction_id.to_string())?;
    transactions.sync_all()?;
    Ok(())
}

/// Remove only bounded, structurally authenticated debris from transactions
/// that crashed before their journal was published. Such a transaction is
/// either empty (the directory was made durable before progress creation) or
/// contains exactly the newly-created, empty progress log. Staging starts only
/// after journal publication. Any other entry is preserved and fails closed.
fn cleanup_orphan_transactions(root: &Path) -> Result<()> {
    let control = open_directory(&root.join(".wheat"))?;
    let Some(transactions) = open_dir_at_optional(&control, TRANSACTIONS)? else {
        return Ok(());
    };
    let entries = sys::read_open_directory(&transactions)
        .context("list the transactions directory")?
        .take(MAX_ORPHAN_TRANSACTIONS + 1)
        .collect::<io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= MAX_ORPHAN_TRANSACTIONS,
        "too many orphan materialization transactions to inspect safely"
    );
    for entry in entries {
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("non-UTF-8 transaction directory name")?;
        let id = Uuid::parse_str(name).context("unexpected materialization transaction entry")?;
        let tx = open_dir_at(&transactions, name)?;
        match orphan_transaction_shape(&tx)? {
            OrphanTransactionShape::Empty => {}
            OrphanTransactionShape::EmptyProgress => {
                let mut progress = open_progress(&tx)?;
                let length = usize::try_from(progress.metadata()?.len())?;
                if length != 0 {
                    ensure!(
                        length % PROGRESS_RECORD_BYTES == 0,
                        "orphan materialization transaction has torn progress"
                    );
                    progress.seek(SeekFrom::End(-(PROGRESS_RECORD_BYTES as i64)))?;
                    let mut terminal = [0_u8; PROGRESS_RECORD_BYTES];
                    progress.read_exact(&mut terminal)?;
                    let terminal = decode_progress_record(&terminal)
                        .context("orphan materialization transaction is not durably terminal")?;
                    let count = usize::try_from(terminal.index)?;
                    let installs = match terminal.opcode {
                        PROGRESS_OPCODE_COMPLETE => count,
                        // The journal is gone, so its total install count is unavailable.
                        // One more than the authenticated intent prefix is a safe bound
                        // that validates the abort terminal without authorizing debris.
                        PROGRESS_OPCODE_ABORTED => count.saturating_add(1),
                        _ => bail!("orphan materialization transaction has no terminal record"),
                    };
                    let state = read_progress(&mut progress, id, installs)?;
                    // Reading truncates a torn final record; a terminal record that
                    // survives intact leaves the log at its full length.
                    ensure!(
                        (state.complete || state.aborted)
                            && usize::try_from(progress.metadata()?.len())? == length,
                        "orphan materialization transaction is not durably terminal"
                    );
                }
            }
        }
        cleanup_abandoned_transaction(root, id, &tx)?;
    }
    Ok(())
}

fn authenticate_replaced_directory(
    root: &Path,
    path: &str,
    replacement: &ReplacedDirectoryPlan,
    restore_consent: bool,
) -> Result<()> {
    ensure!(
        !replacement.expected_leaves.is_empty(),
        "replacement directory has no tracked descendants"
    );
    inventory_directory(root, path, replacement, restore_consent)
}

fn inventory_directory(
    root: &Path,
    path: &str,
    expected: &ReplacedDirectoryPlan,
    restore_consent: bool,
) -> Result<()> {
    for entry in fs::read_dir(root.join(path)).context("inventory replacement directory")? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 entry in replacement directory"))?;
        let relative = format!("{path}/{name}");
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            if !expected.expected_directories.contains(relative.as_str()) {
                return Err(structural_inventory_invalidated(
                    path,
                    &relative,
                    restore_consent,
                ));
            }
            inventory_directory(root, &relative, expected, restore_consent)?;
        } else if !expected.expected_leaves.contains(relative.as_str()) {
            return Err(structural_inventory_invalidated(
                path,
                &relative,
                restore_consent,
            ));
        }
    }
    Ok(())
}

fn structural_inventory_invalidated(
    directory: &str,
    entry: &str,
    restore_consent: bool,
) -> anyhow::Error {
    let message = format!(
        "refusing to replace workspace directory {directory:?} containing untracked entry {entry:?}"
    );
    if restore_consent {
        restore_invalidated(message)
    } else {
        anyhow::anyhow!(message)
    }
}

fn directory_is_empty(root: &Path, path: &str) -> Result<bool> {
    let (parent, name) = open_parent(root, path)?;
    let directory = open_dir_at(&parent, name)?;
    Ok(sys::read_open_directory(&directory)?
        .next()
        .transpose()?
        .is_none())
}

fn path_is_directory(root: &Path, path: &str) -> Result<bool> {
    let Some((parent_path, name)) = path.rsplit_once('/') else {
        return entry_is_directory(&open_directory(root)?, path);
    };
    let parent = match open_directory(&root.join(parent_path)) {
        Ok(parent) => parent,
        Err(error)
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                )
            }) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    entry_is_directory(&parent, name)
}

fn entry_is_directory(parent: &File, name: &str) -> Result<bool> {
    Ok(sys::entry_kind_at(parent, name)? == Some(sys::EntryKind::Directory))
}

fn is_directory_not_empty(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(sys::is_directory_not_empty)
}

fn remove_replaced_directory(
    root: &Path,
    _tx: &File,
    install: &Install,
    planner: &PlannerContext,
) -> Result<()> {
    if install.old.is_some() || install.new.is_none() || !path_is_directory(root, &install.path)? {
        return Ok(());
    }
    ensure!(
        planner
            .replaced_directories
            .contains_key(install.path.as_str()),
        "refusing to remove an unauthenticated workspace directory {:?}",
        install.path
    );
    // Descendants have already been durably exchanged into transaction leaves at
    // this point. A nonempty directory is therefore preserved and recovery retains
    // those leaves rather than deleting concurrent workspace data.
    let (parent, name) = open_parent(root, &install.path)?;
    checkpoint("before-replaced-directory-removal")?;
    match remove_directory_if_exists(&parent, name) {
        Ok(()) => parent.sync_all()?,
        Err(error) if is_directory_not_empty(&error) => {
            return Err(invalidated_install(
                install,
                format!(
                    "refusing to replace nonempty workspace directory {:?}",
                    install.path
                ),
            ));
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("remove authenticated empty directory {:?}", install.path)
            });
        }
    }
    Ok(())
}

fn remove_empty_owned_parents(root: &Path, path: &str, planner: &PlannerContext) -> Result<()> {
    let mut parent = Path::new(path).parent();
    while let Some(relative_path) = parent {
        if relative_path.as_os_str().is_empty() {
            break;
        }
        let relative = relative_path
            .to_str()
            .context("non-UTF-8 materialization parent")?;
        if !planner.source_directories.contains(relative)
            || planner.target_directories.contains(relative)
        {
            break;
        }
        let (directory, name) = open_parent(root, relative)?;
        match remove_directory_if_exists(&directory, name) {
            Ok(()) => directory.sync_all()?,
            Err(error) if is_directory_not_empty(&error) => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove empty plan-owned directory {relative:?}"));
            }
        }
        parent = relative_path.parent();
    }
    Ok(())
}

fn ensure_parent_directories(root: &Path, path: &str) -> Result<()> {
    let mut directory = open_directory(root)?;
    let components = path.split('/').collect::<Vec<_>>();
    for component in &components[..components.len() - 1] {
        match open_dir_at_optional(&directory, component)? {
            Some(next) => directory = next,
            None => {
                mkdirat_new(&directory, component, 0o755)?;
                directory.sync_all()?;
                directory = open_dir_at(&directory, component)?;
            }
        }
    }
    Ok(())
}

fn open_parent<'a>(root: &Path, path: &'a str) -> Result<(File, &'a str)> {
    let mut components = path.split('/').collect::<Vec<_>>();
    let name = components.pop().context("empty materialization path")?;
    let mut directory = open_directory(root)?;
    for component in components {
        directory = open_dir_at(&directory, component)?;
    }
    Ok((directory, name))
}

fn rename_noreplace(
    source: &File,
    source_name: &str,
    destination: &File,
    destination_name: &str,
) -> Result<()> {
    sys::rename_at(
        source,
        source_name,
        destination,
        destination_name,
        sys::Rename::NoReplace,
    )
    .context("install without replacement")
}

fn rename_exchange(
    source: &File,
    source_name: &str,
    destination: &File,
    destination_name: &str,
) -> Result<()> {
    sys::rename_at(
        source,
        source_name,
        destination,
        destination_name,
        sys::Rename::Exchange,
    )
    .context("exchange workspace leaf")
}

fn open_directory(path: &Path) -> Result<File> {
    sys::open_directory(path).context("open directory without following symlinks")
}

fn open_dir_at(directory: &File, name: &str) -> Result<File> {
    open_dir_at_optional(directory, name)?.context("required directory is missing")
}

fn open_dir_at_optional(directory: &File, name: &str) -> Result<Option<File>> {
    match sys::open_directory_at(directory, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("open child directory without following symlinks"),
    }
}

fn mkdirat_new(directory: &File, name: &str, mode: u32) -> Result<()> {
    sys::make_directory_at(directory, name, mode).context("create workspace directory")
}

fn mkdirat_if_missing(directory: &File, name: &str, mode: u32) -> Result<()> {
    match mkdirat_new(directory, name, mode) {
        Ok(()) => directory.sync_all().map_err(Into::into),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) =>
        {
            open_dir_at(directory, name)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn create_regular_at(directory: &File, name: &str, mode: u32) -> Result<File> {
    let how = sys::OpenFile {
        read: false,
        write: true,
        append: false,
        creation: sys::Creation::New,
        mode,
    };
    sys::open_file_at(directory, name, how).context("create private materialization file")
}

fn create_leaf_at(directory: &File, name: &str, kind: FileKind, bytes: &[u8]) -> Result<()> {
    match kind {
        FileKind::Regular | FileKind::Executable => {
            let mode = if kind == FileKind::Executable {
                0o755
            } else {
                0o644
            };
            let mut file = create_regular_at(directory, name, mode)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        FileKind::Symlink => {
            sys::symlink_at(bytes, directory, name).context("stage symlink")?;
        }
    }
    Ok(())
}

fn authenticate_leaf_at_bytes(
    directory: &File,
    name: &str,
    kind: FileKind,
    expected: &[u8],
) -> Result<bool> {
    ensure!(
        expected.len() <= MAX_FILE_BYTES,
        "materialized file exceeds byte limit"
    );
    let entry = leaf_kind_at(directory, name)?;
    if entry == sys::EntryKind::Symlink {
        if kind != FileKind::Symlink || expected.len() > 4_095 {
            return Ok(false);
        }
        let mut bytes = [0_u8; 4_096];
        let read = sys::read_link_at(directory, name, &mut bytes).context("read staged symlink")?;
        return Ok(&bytes[..read] == expected);
    }
    if entry != sys::EntryKind::Regular || kind == FileKind::Symlink {
        return Ok(false);
    }
    let mut file =
        sys::open_file_at(directory, name, sys::OpenFile::READ).context("open staged file")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected.len() as u64 {
        return Ok(false);
    }
    let actual_kind = if sys::is_executable(&metadata) {
        FileKind::Executable
    } else {
        FileKind::Regular
    };
    if actual_kind != kind {
        return Ok(false);
    }
    let mut scratch = [0_u8; 64 * 1024];
    let mut offset = 0_usize;
    loop {
        let read = file.read(&mut scratch)?;
        if read == 0 {
            break;
        }
        let end = offset
            .checked_add(read)
            .context("staged file grew while authenticating")?;
        if end > expected.len() || scratch[..read] != expected[offset..end] {
            return Ok(false);
        }
        offset = end;
    }
    Ok(offset == expected.len())
}

fn read_leaf_at(directory: &File, name: &str, limit: usize) -> Result<WorkspaceFileSnapshot> {
    let entry = leaf_kind_at(directory, name)?;
    if entry == sys::EntryKind::Symlink {
        let mut bytes = vec![0_u8; limit.saturating_add(1)];
        let read = sys::read_link_at(directory, name, &mut bytes).context("read staged symlink")?;
        ensure!(read <= limit, "materialized symlink exceeds byte limit");
        bytes.truncate(read);
        return Ok(WorkspaceFileSnapshot {
            kind: FileKind::Symlink,
            bytes,
        });
    }
    ensure!(
        entry == sys::EntryKind::Regular,
        "staged target is not a regular file or symlink"
    );
    let file =
        sys::open_file_at(directory, name, sys::OpenFile::READ).context("open staged file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "opened staged target is not a regular file"
    );
    ensure!(
        metadata.len() <= u64::try_from(limit)?,
        "materialized file exceeds byte limit"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    file.take(u64::try_from(limit)?.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "materialized file exceeds byte limit");
    let kind = if sys::is_executable(&metadata) {
        FileKind::Executable
    } else {
        FileKind::Regular
    };
    Ok(WorkspaceFileSnapshot { kind, bytes })
}

/// What a staged leaf is. A missing leaf is an error.
fn leaf_kind_at(directory: &File, name: &str) -> Result<sys::EntryKind> {
    sys::entry_kind_at(directory, name)
        .and_then(|entry| entry.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
        .context("stat staged materialization leaf")
}

fn leaf_exists(directory: &File, name: &str) -> Result<bool> {
    Ok(sys::entry_kind_at(directory, name)?.is_some())
}

enum OrphanTransactionShape {
    Empty,
    EmptyProgress,
}

fn orphan_transaction_shape(directory: &File) -> Result<OrphanTransactionShape> {
    let mut entries =
        sys::read_open_directory(directory).context("list a materialization transaction")?;
    let first = entries.next().transpose()?;
    let second = entries.next().transpose()?;
    match (first, second) {
        (None, None) => Ok(OrphanTransactionShape::Empty),
        (Some(entry), None) if entry.file_name() == PROGRESS_NAME => {
            Ok(OrphanTransactionShape::EmptyProgress)
        }
        _ => bail!("materialization transaction contains unexpected debris"),
    }
}

fn ensure_progress_only(directory: &File) -> Result<()> {
    let mut entries =
        sys::read_open_directory(directory).context("list a materialization transaction")?;
    let only = entries
        .next()
        .transpose()?
        .is_some_and(|entry| entry.file_name() == PROGRESS_NAME)
        && entries.next().is_none();
    ensure!(
        only,
        "materialization transaction contains unexpected debris"
    );
    Ok(())
}

fn remove_file_if_exists(directory: &File, name: &str) -> Result<()> {
    ignore_not_found(sys::remove_file_at(directory, name))
}

fn remove_directory_if_exists(directory: &File, name: &str) -> Result<()> {
    ignore_not_found(sys::remove_directory_at(directory, name))
}

fn ignore_not_found(result: io::Result<()>) -> Result<()> {
    match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn checkpoint(name: &str) -> Result<()> {
    let fail_selected = std::env::var_os(TEST_CHECKPOINT_ENV);
    let pause_selected = std::env::var_os(TEST_PAUSE_ENV);
    if fail_selected.as_deref() != Some(OsStr::new(name))
        && pause_selected.as_deref() != Some(OsStr::new(name))
    {
        return Ok(());
    }
    let occurrence = CHECKPOINT_OCCURRENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let requested = std::env::var(TEST_CHECKPOINT_OCCURRENCE_ENV)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .context("invalid checkpoint occurrence")
        })
        .transpose()?
        .unwrap_or(1);
    if occurrence != requested {
        return Ok(());
    }
    if pause_selected.as_deref() == Some(OsStr::new(name)) {
        let pause_file = std::env::var_os(TEST_PAUSE_FILE_ENV)
            .context("materialization pause requires a pause file")?;
        let reached = std::path::PathBuf::from(&pause_file).with_extension("reached");
        fs::write(&reached, b"reached\n")?;
        while Path::new(&pause_file).exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if fail_selected.as_deref() != Some(OsStr::new(name)) {
        return Ok(());
    }
    let mode = std::env::var(TEST_CHECKPOINT_MODE_ENV);
    match mode.as_deref() {
        Ok("exit") => sys::exit_immediately(86),
        Ok("sigkill") => sys::kill_self(),
        Ok(other) => bail!("invalid checkpoint failure mode {other:?}"),
        Err(std::env::VarError::NotPresent) => {
            bail!("injected materialization interruption after {name}")
        }
        Err(error) => bail!("read checkpoint failure mode: {error}"),
    }
}

/// Materializes a resolved frontier into the workspace at `workspace`.
pub fn materialize_resolved(
    workspace: &Path,
    explicit_store: Option<&Path>,
    label: String,
    resolved: crate::frontier_service::ResolvedFrontier,
    dot_git: DotGitGate,
) -> Result<(ViewId, std::path::PathBuf)> {
    let mut context = crate::discovery::resolve_context(workspace, explicit_store)?;
    let repository = crate::local::open(&context.store_root)?;
    let view = resolved.effective_view();
    let materialized = crate::local::runtime()?.block_on(materialize(
        &context.root,
        &repository,
        &mut context.workspace,
        resolved.head,
        label,
        resolved.basis,
        resolved.selected_frontier,
        resolved.tree,
        dot_git,
    ));
    if let Err(error) = materialized {
        // The workspace already exists, so running the same command again would
        // fail; say how to finish this one instead.
        if matches!(
            error.downcast_ref::<crate::outcome::CliFailure>(),
            Some(crate::outcome::CliFailure::DotGitPaths(_))
        ) {
            return Err(crate::outcome::CliFailure::dot_git(format!(
                "{error}\nthe workspace {} is set up but its files were not written; \
                 to write them, run `wheat --workspace {} materialize --allow-dot-git`",
                context.root.display(),
                context.root.display()
            )));
        }
        return Err(error);
    }
    Ok((view, context.root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_and_unsafe_paths_are_rejected() {
        for path in [
            "",
            "/x",
            "../x",
            "a/../x",
            ".wheat/x",
            ".wheat-git/x",
            "repository.head",
            "repository.lock",
            "storage/x",
        ] {
            assert!(validate_path(path).is_err(), "{path:?}");
        }
        validate_path("src/main.rs").unwrap();
    }

    fn open_descriptors() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    /// Listing a transaction directory closes its duplicate descriptor even when the
    /// listing fails. A regular file in place of the directory makes it fail.
    #[test]
    fn transaction_listing_closes_its_descriptor_on_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-a-directory");
        std::fs::write(&path, b"").unwrap();
        let file = File::open(&path).unwrap();

        let before = open_descriptors();
        for _ in 0..8 {
            assert!(ensure_progress_only(&file).is_err());
            assert!(orphan_transaction_shape(&file).is_err());
        }
        assert_eq!(open_descriptors(), before);

        // And on success.
        let transaction = File::open(directory.path()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(directory.path().join(PROGRESS_NAME), b"").unwrap();
        let before = open_descriptors();
        ensure_progress_only(&transaction).unwrap();
        assert_eq!(open_descriptors(), before);
    }

    /// Sizing a regular file reads only stored length metadata: a delta whose base is
    /// gone still sizes, although reading it would fail. Symlink targets are still read
    /// and checked.
    #[test]
    fn materializable_length_sizes_files_without_reading_them() {
        let directory = tempfile::tempdir().unwrap();
        let content = ContentStore::new(directory.path());
        let base = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let target = format!("{base}one more line\n");
        let base_id = content.put(base.as_bytes()).unwrap();
        let target = wheat_core::VerifiedContent::new(target.as_bytes());
        let target_id = target.id();
        content.put_with_base(target, base_id).unwrap();
        std::fs::remove_file(content.path(base_id)).unwrap();
        let regular = |content| FileValue {
            content,
            kind: FileKind::Regular,
        };
        assert!(content.get(target_id).is_err());
        assert_eq!(
            materializable_length(&content, regular(target_id)).unwrap(),
            target.len() as u64
        );

        let symlink = |bytes: &[u8]| FileValue {
            content: content.put(bytes).unwrap(),
            kind: FileKind::Symlink,
        };
        assert_eq!(
            materializable_length(&content, symlink(b"../target")).unwrap(),
            9
        );
        for bad in [&b""[..], b"a\0b", &[b'a'; 4_096]] {
            let error = materializable_length(&content, symlink(bad)).unwrap_err();
            assert!(
                error.to_string().contains("symlink target"),
                "{bad:?}: {error}"
            );
        }
    }

    #[test]
    fn progress_log_is_fixed_width_and_recovers_a_torn_tail() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        append_install_intents(&mut progress, id, 2).unwrap();
        append_install_intents(&mut progress, id, 5).unwrap();
        progress.write_all(b"torn").unwrap();
        progress.sync_data().unwrap();

        assert_eq!(
            read_progress(&mut progress, id, 6).unwrap(),
            ProgressState {
                intents: 5,
                batch_start: 2,
                complete: false,
                aborted: false
            }
        );
        assert_eq!(
            progress.metadata().unwrap().len(),
            u64::try_from(2 * PROGRESS_RECORD_BYTES).unwrap()
        );
    }

    #[test]
    fn progress_log_truncates_a_corrupt_final_full_width_record() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        append_install_intents(&mut progress, id, 2).unwrap();
        append_install_intents(&mut progress, id, 5).unwrap();
        let path = root.path().join(PROGRESS_NAME);
        let mut corruptor = fs::OpenOptions::new().write(true).open(path).unwrap();
        corruptor
            .seek(SeekFrom::Start(
                u64::try_from(2 * PROGRESS_RECORD_BYTES - 4).unwrap(),
            ))
            .unwrap();
        corruptor.write_all(&[0, 0, 0, 0]).unwrap();
        corruptor.sync_data().unwrap();
        drop(corruptor);

        assert_eq!(
            read_progress(&mut progress, id, 6).unwrap(),
            ProgressState {
                intents: 2,
                batch_start: 0,
                complete: false,
                aborted: false
            }
        );
        assert_eq!(
            progress.metadata().unwrap().len(),
            u64::try_from(PROGRESS_RECORD_BYTES).unwrap()
        );
    }

    #[test]
    fn planner_context_indexes_large_structural_delta_by_path_depth() {
        const REPLACEMENTS: usize = 20_000;
        const DEPTH: usize = 2;
        let value = FileValue {
            content: wheat_core::ContentId::of(b"content"),
            kind: FileKind::Regular,
        };
        let mut installs = Vec::with_capacity(REPLACEMENTS * 2);
        for index in 0..REPLACEMENTS {
            installs.push(Install {
                path: format!("root-{index:05}/nested/leaf"),
                old: Some(value),
                new: None,
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: String::new(),
                tombstone: Some(Vec::new()),
            });
        }
        for index in 0..REPLACEMENTS {
            installs.push(Install {
                path: format!("root-{index:05}"),
                old: None,
                new: Some(value),
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: String::new(),
                tombstone: None,
            });
        }

        let mut replacement_lookups = 0_usize;
        let planner = PlannerContext::new_with_replacement_lookup(&installs, |_| {
            replacement_lookups += 1;
        });
        assert_eq!(replacement_lookups, REPLACEMENTS * DEPTH);
        assert_eq!(planner.replaced_directories.len(), REPLACEMENTS);
        for index in 0..REPLACEMENTS {
            let replacement_path = format!("root-{index:05}");
            let replacement = planner
                .replaced_directories
                .get(replacement_path.as_str())
                .unwrap();
            assert_eq!(replacement.first_descendant_operation, index);
            assert_eq!(replacement.expected_leaves.len(), 1);
            let expected_leaf = format!("root-{index:05}/nested/leaf");
            assert!(replacement.expected_leaves.contains(expected_leaf.as_str()));
            assert_eq!(replacement.expected_directories.len(), 1);
            let expected_directory = format!("root-{index:05}/nested");
            assert!(
                replacement
                    .expected_directories
                    .contains(expected_directory.as_str())
            );
        }
    }

    #[test]
    fn planner_context_borrows_deep_prefixes_and_uses_nearest_replacement() {
        const DEPTH: usize = 1_024;
        let value = FileValue {
            content: wheat_core::ContentId::of(b"content"),
            kind: FileKind::Regular,
        };
        let mut leaf = String::from("root");
        let mut nearest_replacement = String::new();
        for depth in 0..DEPTH {
            leaf.push('/');
            leaf.push_str("segment");
            if depth + 1 == DEPTH / 2 {
                nearest_replacement.clone_from(&leaf);
            }
        }
        leaf.push_str("/leaf");
        let installs = vec![
            Install {
                path: leaf,
                old: Some(value),
                new: None,
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: String::new(),
                tombstone: Some(Vec::new()),
            },
            Install {
                path: String::from("root"),
                old: None,
                new: Some(value),
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: String::new(),
                tombstone: None,
            },
            Install {
                path: nearest_replacement.clone(),
                old: None,
                new: Some(value),
                observed: None,
                restore_consent: false,
                empty_directory: false,
                staged: String::new(),
                tombstone: None,
            },
        ];
        let planner = PlannerContext::new(&installs);

        assert!(!planner.replaced_directories.contains_key("root"));
        let replacement = planner
            .replaced_directories
            .get(nearest_replacement.as_str())
            .unwrap();
        assert_eq!(replacement.expected_leaves.len(), 1);
        assert_eq!(replacement.expected_directories.len(), DEPTH / 2);

        let points_into_install = |path: &str| {
            let pointer = path.as_ptr() as usize;
            installs.iter().any(|install| {
                let start = install.path.as_ptr() as usize;
                let end = start + install.path.len();
                pointer >= start && pointer + path.len() <= end
            })
        };
        assert!(
            planner
                .removed_paths
                .iter()
                .copied()
                .all(points_into_install)
        );
        assert!(
            planner
                .replaced_directories
                .keys()
                .copied()
                .all(points_into_install)
        );
        assert!(
            planner
                .source_directories
                .iter()
                .copied()
                .all(points_into_install)
        );
        assert!(
            planner
                .target_directories
                .iter()
                .copied()
                .all(points_into_install)
        );
        assert!(
            replacement
                .expected_leaves
                .iter()
                .copied()
                .all(points_into_install)
        );
        assert!(
            replacement
                .expected_directories
                .iter()
                .copied()
                .all(points_into_install)
        );
    }

    #[test]
    fn restore_indices_check_only_path_ancestors_at_representative_scale() {
        const LEAVES: usize = 20_000;
        const CHECKS_TO_PINNED_BOUNDARY: usize = 2;
        const FULL_DEPTH: usize = 3;
        let value = FileValue {
            content: wheat_core::ContentId::of(b"content"),
            kind: FileKind::Regular,
        };
        let selectors = (0..LEAVES)
            .map(|index| format!("root-{index:05}/pinned"))
            .collect::<Vec<_>>();
        let selected = (0..LEAVES)
            .map(|index| (format!("root-{index:05}/pinned/nested/leaf"), value))
            .collect::<std::collections::BTreeMap<_, _>>();

        let mut representative_checks = 0_usize;
        let representatives = descendant_representatives(&selectors, &selected, |_| {
            representative_checks += 1;
        });
        assert_eq!(representative_checks, LEAVES * FULL_DEPTH);
        assert_eq!(representatives.len(), LEAVES);

        let obstructions = selectors.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut obstruction_checks = 0_usize;
        for path in selected.keys() {
            assert!(is_shadowed_by_exact_obstruction(
                path,
                &obstructions,
                |_| obstruction_checks += 1,
            ));
        }
        assert_eq!(obstruction_checks, LEAVES * CHECKS_TO_PINNED_BOUNDARY);
    }

    #[test]
    fn operation_content_residency_counter_records_real_owned_buffers() {
        TEST_PEAK_OWNED_CONTENT_BYTES.store(0, Ordering::Relaxed);
        let old = [0_u8; 17];
        let new = [0_u8; 29];
        let resident = old.len() + new.len();
        TEST_PEAK_OWNED_CONTENT_BYTES.fetch_max(resident, Ordering::Relaxed);
        assert_eq!(TEST_PEAK_OWNED_CONTENT_BYTES.load(Ordering::Relaxed), 46);
        assert!(resident as u64 <= MAX_OPERATION_RESIDENT_BYTES);
    }

    #[test]
    fn batches_of_large_files_hold_at_most_the_batch_bound() {
        const FILE_BYTES: usize = 12 * 1024 * 1024;
        const BIG_BYTES: usize = 80 * 1024 * 1024;
        let root = tempfile::tempdir().unwrap();
        let objects = wheat_store::PackedObjectStore::open(root.path().join("storage")).unwrap();
        let content = objects.content_store();
        let repository = LocalRepository {
            store_id: Uuid::nil(),
            objects,
            content,
            heads: wheat_store::LocalPublicationStore::new(root.path().join("repository.head")),
        };
        // Twelve 12 MiB files with one file over the bound in the middle.
        let sizes = (0..12)
            .map(|index| if index == 6 { BIG_BYTES } else { FILE_BYTES })
            .collect::<Vec<_>>();
        let installs = sizes
            .iter()
            .enumerate()
            .map(|(index, &size)| {
                let bytes = vec![u8::try_from(index).unwrap(); size];
                Install {
                    path: format!("data/f{index:02}.bin"),
                    old: None,
                    new: Some(FileValue {
                        content: repository.content.put(&bytes).unwrap(),
                        kind: FileKind::Regular,
                    }),
                    observed: None,
                    restore_consent: false,
                    empty_directory: false,
                    staged: format!("staged-{index:02}"),
                    tombstone: None,
                }
            })
            .collect::<Vec<_>>();

        let mut batcher = Batcher {
            repository: &repository,
            installs: &installs,
            next: 0,
            carried: None,
        };
        let mut batches = Vec::new();
        loop {
            let batch = batcher.next_batch().unwrap();
            if batch.is_empty() {
                break;
            }
            let resident = batch
                .iter()
                .map(|loaded| loaded.resident_bytes)
                .sum::<u64>();
            assert!(
                batch.len() == 1 || resident <= MAX_BATCH_RESIDENT_BYTES,
                "a batch of {} installs holds {resident} bytes",
                batch.len()
            );
            batches.push(batch.iter().map(|loaded| loaded.index).collect::<Vec<_>>());
        }
        // Five 12 MiB files fit in 64 MiB; the 80 MiB file goes alone.
        assert_eq!(
            batches,
            [vec![0, 1, 2, 3, 4], vec![5], vec![6], vec![7, 8, 9, 10, 11]]
        );
    }

    #[test]
    fn completion_record_is_unique_final_and_follows_all_intents() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        append_install_intents(&mut progress, id, 1).unwrap();
        append_completion(&mut progress, id, 2).unwrap();
        assert_eq!(
            read_progress(&mut progress, id, 2).unwrap(),
            ProgressState {
                intents: 1,
                batch_start: 0,
                complete: false,
                aborted: false
            }
        );

        progress.set_len(PROGRESS_RECORD_BYTES as u64).unwrap();
        progress.seek(SeekFrom::End(0)).unwrap();
        append_install_intents(&mut progress, id, 2).unwrap();
        append_completion(&mut progress, id, 2).unwrap();
        let complete = ProgressState {
            intents: 2,
            batch_start: 1,
            complete: true,
            aborted: false,
        };
        assert_eq!(read_progress(&mut progress, id, 2).unwrap(), complete);
        append_completion(&mut progress, id, 2).unwrap();
        assert_eq!(read_progress(&mut progress, id, 2).unwrap(), complete);
        assert_eq!(
            progress.metadata().unwrap().len(),
            u64::try_from(3 * PROGRESS_RECORD_BYTES).unwrap()
        );
    }

    #[test]
    fn a_batch_record_must_move_forward_by_at_most_one_batch() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let installs = 3 * MAX_BATCH_INSTALLS;
        for ends in [
            [MAX_BATCH_INSTALLS, MAX_BATCH_INSTALLS],
            [MAX_BATCH_INSTALLS, MAX_BATCH_INSTALLS - 1],
            [MAX_BATCH_INSTALLS, 2 * MAX_BATCH_INSTALLS + 1],
            [MAX_BATCH_INSTALLS, installs + 1],
        ] {
            let mut progress = create_progress(&tx).unwrap();
            for end in ends {
                append_install_intents(&mut progress, id, end).unwrap();
            }
            // A bad final record reads as torn, so add a valid one after it.
            append_completion(&mut progress, id, installs).unwrap();
            assert!(
                read_progress(&mut progress, id, installs).is_err(),
                "{ends:?}"
            );
            drop(progress);
            fs::remove_file(root.path().join(PROGRESS_NAME)).unwrap();
        }
        let mut progress = create_progress(&tx).unwrap();
        append_install_intents(&mut progress, id, MAX_BATCH_INSTALLS + 1).unwrap();
        append_completion(&mut progress, id, installs).unwrap();
        assert!(read_progress(&mut progress, id, installs).is_err());
    }

    #[test]
    fn path_ancestors_lists_proper_ancestors_nearest_first() {
        assert_eq!(path_ancestors("a/b/c").collect::<Vec<_>>(), ["a/b", "a"]);
        assert_eq!(path_ancestors("a").count(), 0);
    }

    #[test]
    fn bounded_journal_writer_refuses_growth_past_limit() {
        let mut writer = BoundedJournalWriter { bytes: Vec::new() };
        writer.bytes.resize(MAX_JOURNAL_BYTES - 2, 0);
        assert_eq!(writer.write(&[1]).unwrap(), 1);
        let error = writer.write(&[2]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(writer.bytes.len(), MAX_JOURNAL_BYTES - 1);
    }

    #[test]
    fn selector_validation_rejects_noncanonical_and_oversized_inputs() {
        assert!(validate_selectors(&[]).is_err());
        assert!(validate_selectors(&[String::from("b"), String::from("a")]).is_err());
        assert!(validate_selectors(&[String::from("a"), String::from("a")]).is_err());
        assert!(validate_selectors(&["x".repeat(MAX_PATH_BYTES + 1)]).is_err());
        validate_selectors(&[String::from("a"), String::from("a/b"), String::from("b")]).unwrap();
    }

    #[test]
    fn progress_log_rejects_corrupt_interior_records() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        for end in 1..=3 {
            append_install_intents(&mut progress, id, end).unwrap();
        }
        drop(progress);
        let path = root.path().join(PROGRESS_NAME);
        let mut corruptor = fs::OpenOptions::new().write(true).open(&path).unwrap();
        corruptor.seek(SeekFrom::Start(8)).unwrap();
        corruptor.write_all(&[0]).unwrap();
        corruptor.sync_data().unwrap();
        drop(corruptor);
        let mut progress = open_progress(&tx).unwrap();

        assert!(read_progress(&mut progress, id, 3).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_replace_rename_preserves_a_concurrent_destination() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        fs::write(root.path().join("destination"), b"external").unwrap();
        let directory = open_directory(root.path()).unwrap();
        assert!(rename_noreplace(&directory, "source", &directory, "destination").is_err());
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("destination")).unwrap(),
            b"external"
        );
    }
}
