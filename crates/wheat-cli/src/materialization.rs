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
    ffi::{CString, OsStr},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wheat_core::{FileKind, FileValue, TreeId, ViewId};
use wheat_store::{
    MerkleTreeStore, PublicationStore, RepositoryHead, TreeDiffLimits, TreeFile, TreeWalkLimits,
};

use crate::{
    local::LocalRepository,
    workspace_files::{WorkspaceFileSnapshot, authenticate_workspace_file, read_workspace_file},
    workspace_metadata::{
        MaterializedState, WorkspaceMetadata, WorkspaceMetadataPublishError, WorkspaceSelection,
    },
};

const VERSION: u32 = 2;
const JOURNAL_NAME: &str = "materialization.json";
const TRANSACTIONS: &str = "materialization-transactions";
const PROGRESS_NAME: &str = "progress.log";
const PROGRESS_MAGIC: &[u8; 4] = b"WMI1";
const PROGRESS_RECORD_BYTES: usize = 36;
const MAX_JOURNAL_BYTES: usize = 192 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
// ContentStore currently returns owned Vecs. A replacement needs old and new
// bytes concurrently to authenticate both sides of an exchange; workspace,
// staged, and displaced regular files are streamed through a fixed 64 KiB
// scratch buffer and add no file-sized allocation. Thus peak owned file content
// is exactly two bounded store buffers (one when content IDs are equal).
const MAX_OPERATION_RESIDENT_BYTES: u64 = 2 * MAX_FILE_BYTES as u64;
#[cfg(test)]
static TEST_PEAK_OWNED_CONTENT_BYTES: AtomicUsize = AtomicUsize::new(0);
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TEST_CHECKPOINT_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_AFTER";
const TEST_CHECKPOINT_OCCURRENCE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_OCCURRENCE";
const TEST_CHECKPOINT_MODE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_MODE";
const TEST_PAUSE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_PAUSE_AT";
const TEST_PAUSE_FILE_ENV: &str = "WHEAT_TEST_MATERIALIZATION_PAUSE_FILE";
const MAX_ORPHAN_TRANSACTIONS: usize = 1_024;
static CHECKPOINT_OCCURRENCE: AtomicUsize = AtomicUsize::new(0);
const RESERVED: &[&str] = &[
    ".wheat",
    ".wheat-git",
    "repository.head",
    "repository.lock",
    "storage",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    path: String,
    old: Option<FileValue>,
    new: Option<FileValue>,
    staged: String,
    tombstone: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    transaction_id: Uuid,
    workspace_id: Uuid,
    store_id: Uuid,
    source: Option<MaterializedState>,
    selection: WorkspaceSelection,
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

pub(crate) struct WorkspaceLock(File);

impl WorkspaceLock {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        bail!("Wheat workspace materialization is currently supported only on Linux");
        #[cfg(target_os = "linux")]
        {
            let control = open_directory(&root.join(".wheat"))?;
            let name = c_string("workspace.lock")?;
            let fd = unsafe {
                libc::openat(
                    control.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("open Wheat workspace lock");
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    bail!("workspace is locked by another operation");
                }
                return Err(error.into());
            }
            Ok(Self(file))
        }
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) async fn materialize(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
    head: RepositoryHead,
    view: ViewId,
    tree: TreeId,
    requested_selection: WorkspaceSelection,
) -> Result<()> {
    let _lock = WorkspaceLock::acquire(root)?;
    let persisted = WorkspaceMetadata::read(root)?;
    ensure!(
        persisted.workspace_id == metadata.workspace_id
            && persisted.store == metadata.store
            && persisted.store.store_id == repository.store_id,
        "workspace/store identity changed while materialization was resolved"
    );
    *metadata = persisted;
    recover_locked(root, repository, metadata).await?;

    let target = MaterializedState {
        repository_head: head.into(),
        view,
        tree,
    };
    if metadata.materialized.as_ref() == Some(&target) && metadata.selection == requested_selection
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
                staged: format!("leaf-{index:08x}"),
                tombstone: None,
            })
            .collect()
    };
    let journal = Journal {
        version: VERSION,
        transaction_id,
        workspace_id: metadata.workspace_id,
        store_id: metadata.store.store_id,
        source,
        selection: requested_selection,
        target,
        installs,
    };
    let planner = PlannerContext::new(&journal.installs);
    authenticate_preflight(root, repository, &journal.installs, &planner)?;
    checkpoint("preflight-complete")?;
    ensure!(
        repository.heads.read()? == head,
        "repository head changed while materialization was planned"
    );
    let journal_bytes = encode_journal(&journal)?;
    let (tx, mut progress) = create_transaction(root, transaction_id)?;
    if let Err(error) = write_journal(root, &journal_bytes) {
        if !journal_exists(root)? {
            cleanup_abandoned_transaction(root, transaction_id, &tx)?;
        }
        return Err(error);
    }
    checkpoint("journal-created")?;
    roll_forward(root, repository, &tx, &mut progress, &journal, &planner)?;
    checkpoint("all-installed")?;
    publish_target(root, metadata, &journal)?;
    checkpoint("target-published")?;
    cleanup_committed(root, repository, &journal, &tx)?;
    Ok(())
}

pub(crate) async fn recover(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    *metadata = WorkspaceMetadata::read(root)?;
    recover_locked(root, repository, metadata).await
}

async fn recover_locked(
    root: &Path,
    repository: &LocalRepository,
    metadata: &mut WorkspaceMetadata,
) -> Result<()> {
    let Some(journal) = read_journal(root)? else {
        cleanup_orphan_transactions(root)?;
        return Ok(());
    };
    authenticate_journal(repository, metadata, &journal).await?;
    let (tx, mut progress) = open_transaction(root, journal.transaction_id)?;
    if metadata.materialized.as_ref() == Some(&journal.target) {
        return cleanup_committed(root, repository, &journal, &tx);
    }
    ensure!(
        metadata.materialized == journal.source,
        "materialization journal conflicts with workspace metadata"
    );
    let planner = PlannerContext::new(&journal.installs);
    roll_forward(root, repository, &tx, &mut progress, &journal, &planner)?;
    publish_target(root, metadata, &journal)?;
    checkpoint("target-published")?;
    cleanup_committed(root, repository, &journal, &tx)
}

fn publish_target(root: &Path, metadata: &mut WorkspaceMetadata, journal: &Journal) -> Result<()> {
    let mut next = metadata.clone();
    next.selection = journal.selection.clone();
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
    for file in &files {
        validate_path(&file.path)?;
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
            let bytes = repository.content.get(value.content)?;
            ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "materialized file exceeds byte limit"
            );
            total = total
                .checked_add(u64::try_from(bytes.len())?)
                .context("materialization byte count overflow")?;
            ensure!(
                total <= MAX_TOTAL_BYTES,
                "materialization exceeds total byte limit"
            );
            if value.kind == FileKind::Symlink {
                ensure!(
                    !bytes.is_empty() && !bytes.contains(&0) && bytes.len() <= 4_095,
                    "materialized symlink target is not representable"
                );
            }
        }
        let index = installs.len();
        installs.push(Install {
            path: change.path,
            old: change.old,
            new: change.new,
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
    authenticate_target(repository, journal).await?;
    let expected = match &journal.source {
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
                staged: format!("leaf-{index:08x}"),
                tombstone: None,
            })
            .collect(),
    };
    ensure!(
        journal.installs == expected,
        "journal does not match authenticated tree delta"
    );
    Ok(())
}

async fn authenticate_target(repository: &LocalRepository, journal: &Journal) -> Result<()> {
    let head: RepositoryHead = journal.target.repository_head.clone().into();
    let reader = wheat_store::RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.validate_envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let selected = match &journal.selection {
        WorkspaceSelection::Published => state.published_view,
        WorkspaceSelection::Working => state.working_view,
        WorkspaceSelection::Branch { name } => {
            let branches = reader.branch_manifest(envelope.branches).await?;
            *branches
                .branches
                .get(name)
                .with_context(|| format!("journal references unknown Wheat branch {name}"))?
        }
    };
    ensure!(
        selected == journal.target.view,
        "journal selection does not resolve to target view"
    );
    ensure!(
        reader.view(selected).await?.tree == journal.target.tree,
        "journal target view does not resolve to target tree"
    );
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.starts_with('/') && !path.as_bytes().contains(&0),
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
        let bytes = repository.content.get(file.value.content)?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "materialized file exceeds byte limit"
        );
        total = total
            .checked_add(u64::try_from(bytes.len())?)
            .context("materialization byte count overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES,
            "materialization exceeds total byte limit"
        );
        if file.value.kind == FileKind::Symlink {
            ensure!(
                !bytes.is_empty() && !bytes.contains(&0) && bytes.len() <= 4_095,
                "materialized symlink target is not representable"
            );
        }
    }
    Ok(())
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

fn authenticate_preflight(
    root: &Path,
    repository: &LocalRepository,
    installs: &[Install],
    planner: &PlannerContext,
) -> Result<()> {
    for install in installs {
        if planner.is_shadowed_by_removed_leaf(&install.path) {
            continue;
        }
        if let Some(replacement) = planner.replaced_directories.get(install.path.as_str()) {
            if !path_is_directory(root, &install.path)? {
                // The planned replacement is already absent or a leaf.
            } else {
                authenticate_replaced_directory(root, &install.path, replacement)?;
                continue;
            }
        }
        let old_content = install
            .old
            .map(|value| repository.content.get(value.content))
            .transpose()?;
        let expected = install
            .old
            .zip(old_content.as_deref())
            .map(|(value, bytes)| (value.kind, bytes));
        ensure!(
            authenticate_workspace_file(root, Path::new(&install.path), expected, MAX_FILE_BYTES)
                .with_context(|| format!("preflight workspace path {:?}", install.path))?,
            "workspace path {:?} is dirty or untracked",
            install.path
        );
    }
    Ok(())
}

fn roll_forward(
    root: &Path,
    repository: &LocalRepository,
    tx: &File,
    progress: &mut File,
    journal: &Journal,
    planner: &PlannerContext,
) -> Result<()> {
    let completed = read_progress(progress, journal.transaction_id, journal.installs.len())?;
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
            authenticate_replaced_directory(root, directory, replacement)?;
        }
    }
    checkpoint("structural-recheck-complete")?;
    // Intent records are contiguous and each operation is fully settled before the
    // next record. Only the final recorded operation can have been interrupted.
    let resume_at = completed.saturating_sub(1);
    for (index, install) in journal.installs.iter().enumerate().skip(resume_at) {
        // Content residency is bounded to this operation. If old and new have the
        // same content ID, retain one copy and share it between both descriptors.
        let old_content = install
            .old
            .map(|value| repository.content.get(value.content))
            .transpose()?;
        let shared_content = install
            .old
            .zip(install.new)
            .is_some_and(|(old, new)| old.content == new.content);
        let new_content = if shared_content {
            None
        } else {
            install
                .new
                .map(|value| repository.content.get(value.content))
                .transpose()?
        };
        for bytes in [old_content.as_ref(), new_content.as_ref()]
            .into_iter()
            .flatten()
        {
            ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "materialized file exceeds byte limit"
            );
        }
        let resident_bytes = old_content
            .as_ref()
            .map_or(0_u64, |bytes| bytes.len() as u64)
            .checked_add(
                new_content
                    .as_ref()
                    .map_or(0_u64, |bytes| bytes.len() as u64),
            )
            .context("materialization resident byte count overflow")?;
        ensure!(
            resident_bytes <= MAX_OPERATION_RESIDENT_BYTES,
            "materialization operation exceeds resident byte limit"
        );
        #[cfg(test)]
        TEST_PEAK_OWNED_CONTENT_BYTES.fetch_max(resident_bytes as usize, Ordering::Relaxed);
        let old_bytes = old_content.as_deref();
        let new_bytes = if shared_content {
            old_bytes
        } else {
            new_content.as_deref()
        };
        let has_intent = index < completed;
        if !has_intent {
            prepare_install(root, tx, install, old_bytes, new_bytes, planner)?;
            checkpoint("staged")?;
            append_install_intent(progress, journal.transaction_id, index)?;
            checkpoint("intent-persisted")?;
        }
        apply_or_recover_install(root, tx, install, old_bytes, new_bytes, planner)?;
        checkpoint("installed")?;
    }
    Ok(())
}

fn prepare_install(
    root: &Path,
    tx: &File,
    install: &Install,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    planner: &PlannerContext,
) -> Result<()> {
    remove_replaced_directory(root, tx, install, planner)?;
    let expected = install
        .old
        .zip(old_bytes)
        .map(|(value, bytes)| (value.kind, bytes));
    ensure!(
        authenticate_workspace_file(root, Path::new(&install.path), expected, MAX_FILE_BYTES)?,
        "workspace path {:?} changed after preflight",
        install.path
    );
    if leaf_exists(tx, &install.staged)? {
        match (install.new, new_bytes, install.tombstone.as_deref()) {
            (Some(value), Some(bytes), None) => ensure!(
                authenticate_leaf_at_bytes(tx, &install.staged, value.kind, bytes)?,
                "staged transaction leaf changed before intent"
            ),
            (None, None, Some(marker)) => ensure!(
                authenticate_leaf_at_bytes(tx, &install.staged, FileKind::Regular, marker)?,
                "staged deletion tombstone changed before intent"
            ),
            _ => bail!("invalid materialization operation"),
        }
    } else {
        match (install.new, new_bytes, install.tombstone.as_deref()) {
            (Some(value), Some(bytes), None) => {
                create_leaf_at(tx, &install.staged, value.kind, bytes)?;
            }
            (None, None, Some(marker)) => {
                create_leaf_at(tx, &install.staged, FileKind::Regular, marker)?;
            }
            _ => bail!("invalid materialization operation"),
        }
        tx.sync_all()?;
    }
    ensure_parent_directories(root, &install.path)?;
    Ok(())
}

fn apply_or_recover_install(
    root: &Path,
    tx: &File,
    install: &Install,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    planner: &PlannerContext,
) -> Result<()> {
    match (install.old, install.new) {
        (None, Some(new)) => {
            let (parent, name) = open_parent(root, &install.path)?;
            let new_bytes = new_bytes.context("creation target content is unavailable")?;
            if leaf_exists(tx, &install.staged)? {
                ensure!(
                    read_workspace_file(root, Path::new(&install.path), 0)?.is_none(),
                    "workspace collision while creating {:?}",
                    install.path
                );
                authenticate_exact_at_bytes(tx, &install.staged, new, new_bytes)?;
                rename_noreplace(tx, &install.staged, &parent, name)?;
                parent.sync_all()?;
                tx.sync_all()?;
            }
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
        (Some(old), Some(new)) => {
            let (parent, name) = open_parent(root, &install.path)?;
            let old_bytes = old_bytes.context("replacement preimage content is unavailable")?;
            let new_bytes = new_bytes.context("replacement target content is unavailable")?;
            let workspace_is_old = authenticate_workspace_file(
                root,
                Path::new(&install.path),
                Some((old.kind, old_bytes)),
                MAX_FILE_BYTES,
            )?;
            let staged_is_new =
                authenticate_leaf_at_bytes(tx, &install.staged, new.kind, new_bytes)?;
            if workspace_is_old && staged_is_new {
                checkpoint("before-exchange")?;
                rename_exchange(tx, &install.staged, &parent, name)?;
                parent.sync_all()?;
                tx.sync_all()?;
                checkpoint("exchanged")?;
                if !authenticate_leaf_at_bytes(tx, &install.staged, old.kind, old_bytes)? {
                    if authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((new.kind, new_bytes)),
                        MAX_FILE_BYTES,
                    )? {
                        rename_exchange(tx, &install.staged, &parent, name)?;
                        parent.sync_all()?;
                        tx.sync_all()?;
                        bail!(
                            "workspace path {:?} changed during replacement; restored concurrent data",
                            install.path
                        );
                    }
                    bail!(
                        "workspace path {:?} changed during replacement; preserved both values",
                        install.path
                    );
                }
            } else {
                ensure!(
                    authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((new.kind, new_bytes)),
                        MAX_FILE_BYTES
                    )? && authenticate_leaf_at_bytes(tx, &install.staged, old.kind, old_bytes)?,
                    "ambiguous interrupted replacement for {:?}",
                    install.path
                );
            }
        }
        (Some(old), None) => {
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
            let workspace_is_old = authenticate_workspace_file(
                root,
                Path::new(&install.path),
                Some((old.kind, old_bytes)),
                MAX_FILE_BYTES,
            )?;
            let staged_is_marker =
                authenticate_leaf_at_bytes(tx, &install.staged, FileKind::Regular, marker)?;
            if workspace_is_old && staged_is_marker {
                checkpoint("before-exchange")?;
                rename_exchange(tx, &install.staged, &parent, name)?;
                parent.sync_all()?;
                tx.sync_all()?;
                checkpoint("exchanged")?;
                if !authenticate_leaf_at_bytes(tx, &install.staged, old.kind, old_bytes)? {
                    if authenticate_workspace_file(
                        root,
                        Path::new(&install.path),
                        Some((FileKind::Regular, marker)),
                        MAX_FILE_BYTES,
                    )? {
                        rename_exchange(tx, &install.staged, &parent, name)?;
                        parent.sync_all()?;
                        tx.sync_all()?;
                        bail!(
                            "workspace path {:?} changed during deletion exchange; restored concurrent data",
                            install.path
                        );
                    }
                    bail!(
                        "workspace path {:?} changed during deletion exchange; preserved both values",
                        install.path
                    );
                }
            } else {
                ensure!(
                    authenticate_leaf_at_bytes(tx, &install.staged, old.kind, old_bytes)?
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
                        bail!(
                            "workspace path {:?} changed during deletion move; restored concurrent data",
                            install.path
                        );
                    }
                    bail!(
                        "workspace path {:?} changed during deletion move; preserved concurrent data",
                        install.path
                    );
                }
                unlinkat_if_exists(tx, &removed, 0)?;
                tx.sync_all()?;
            }
            remove_empty_owned_parents(root, &install.path, planner)?;
        }
        _ => bail!("invalid materialization operation"),
    }
    Ok(())
}

fn authenticate_snapshot_exact(
    repository: &LocalRepository,
    value: FileValue,
    actual: &WorkspaceFileSnapshot,
) -> Result<()> {
    let expected = repository.content.get(value.content)?;
    authenticate_snapshot_bytes(value, &expected, actual)
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

fn authenticate_exact_at(
    directory: &File,
    name: &str,
    repository: &LocalRepository,
    value: FileValue,
) -> Result<()> {
    let actual = read_leaf_at(directory, name, MAX_FILE_BYTES)?;
    authenticate_snapshot_exact(repository, value, &actual)
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
    open_progress_with_flags(
        tx,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_APPEND,
    )
}

fn open_progress(tx: &File) -> Result<File> {
    open_progress_with_flags(tx, libc::O_RDWR | libc::O_APPEND)
}

fn open_progress_with_flags(tx: &File, flags: libc::c_int) -> Result<File> {
    let name = c_string(PROGRESS_NAME)?;
    let fd = unsafe {
        libc::openat(
            tx.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open materialization progress log");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    ensure!(
        file.metadata()?.is_file(),
        "materialization progress log is not a regular file"
    );
    Ok(file)
}

fn append_install_intent(progress: &mut File, transaction_id: Uuid, index: usize) -> Result<()> {
    let index = u64::try_from(index)?;
    let mut record = [0_u8; PROGRESS_RECORD_BYTES];
    record[..4].copy_from_slice(PROGRESS_MAGIC);
    record[4..6].copy_from_slice(&1_u16.to_le_bytes());
    record[6..8].copy_from_slice(&1_u16.to_le_bytes());
    record[8..24].copy_from_slice(transaction_id.as_bytes());
    record[24..32].copy_from_slice(&index.to_le_bytes());
    let checksum = crc32fast::hash(&record[..32]);
    record[32..].copy_from_slice(&checksum.to_le_bytes());
    progress.write_all(&record)?;
    progress.sync_data()?;
    Ok(())
}

fn read_progress(progress: &mut File, transaction_id: Uuid, installs: usize) -> Result<usize> {
    let length = usize::try_from(progress.metadata()?.len())?;
    let complete_bytes = length / PROGRESS_RECORD_BYTES * PROGRESS_RECORD_BYTES;
    ensure!(
        complete_bytes / PROGRESS_RECORD_BYTES <= installs,
        "materialization progress exceeds install count"
    );
    progress.seek(SeekFrom::Start(0))?;
    let mut record = [0_u8; PROGRESS_RECORD_BYTES];
    let mut valid = 0usize;
    let complete_records = complete_bytes / PROGRESS_RECORD_BYTES;
    for index in 0..complete_records {
        progress.read_exact(&mut record)?;
        if let Err(error) = validate_progress_record(&record, transaction_id, index) {
            // An interrupted append may leave a full-width record whose payload or
            // checksum never reached stable storage. Only the last physical record
            // can be such a torn append; corruption before it is unambiguously in
            // committed progress and remains a hard error.
            if index + 1 == complete_records {
                break;
            }
            return Err(error)
                .context("materialization progress contains an invalid complete record");
        }
        valid += 1;
    }
    let valid_bytes = valid * PROGRESS_RECORD_BYTES;
    if valid_bytes != length {
        progress.set_len(u64::try_from(valid_bytes)?)?;
        progress.sync_data()?;
    }
    progress.seek(SeekFrom::End(0))?;
    Ok(valid)
}

fn validate_progress_record(
    record: &[u8; PROGRESS_RECORD_BYTES],
    id: Uuid,
    index: usize,
) -> Result<()> {
    ensure!(&record[..4] == PROGRESS_MAGIC, "invalid progress magic");
    ensure!(
        u16::from_le_bytes(record[4..6].try_into()?) == 1,
        "invalid progress version"
    );
    ensure!(
        u16::from_le_bytes(record[6..8].try_into()?) == 1,
        "invalid progress opcode"
    );
    ensure!(
        &record[8..24] == id.as_bytes(),
        "progress transaction mismatch"
    );
    ensure!(
        u64::from_le_bytes(record[24..32].try_into()?) == u64::try_from(index)?,
        "non-contiguous materialization progress"
    );
    ensure!(
        u32::from_le_bytes(record[32..36].try_into()?) == crc32fast::hash(&record[..32]),
        "invalid progress checksum"
    );
    Ok(())
}

fn journal_exists(root: &Path) -> Result<bool> {
    let control = open_directory(&root.join(".wheat"))?;
    leaf_exists(&control, JOURNAL_NAME)
}

fn read_journal(root: &Path) -> Result<Option<Journal>> {
    let control = open_directory(&root.join(".wheat"))?;
    let name = c_string(JOURNAL_NAME)?;
    let fd = unsafe {
        libc::openat(
            control.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error).context("open materialization journal without following symlinks")
        };
    }
    let file = unsafe { File::from_raw_fd(fd) };
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
    Ok(Some(
        serde_json::from_slice(&bytes).context("decode materialization journal")?,
    ))
}

fn encode_journal(journal: &Journal) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(journal)?;
    ensure!(
        bytes.len() < MAX_JOURNAL_BYTES,
        "materialization journal exceeds byte limit"
    );
    bytes.push(b'\n');
    Ok(bytes)
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
        let _ = unlinkat_if_exists(&control, &temporary_name, 0);
    }
    result
}

/// Remove the journal first. A crash can then leave only an empty transaction
/// directory, which is inert and cannot trigger recovery or touch workspace data.
fn cleanup_committed(
    root: &Path,
    repository: &LocalRepository,
    journal: &Journal,
    tx: &File,
) -> Result<()> {
    for install in &journal.installs {
        match install.old {
            Some(old) if leaf_exists(tx, &install.staged)? => {
                authenticate_exact_at(tx, &install.staged, repository, old).with_context(|| {
                    format!("authenticate displaced preimage {:?}", install.path)
                })?;
                unlinkat_if_exists(tx, &install.staged, 0)?;
            }
            _ => {}
        }
    }
    tx.sync_all()?;
    ensure_progress_only(tx)?;
    let control = open_directory(&root.join(".wheat"))?;
    unlinkat_if_exists(&control, JOURNAL_NAME, 0)?;
    control.sync_all()?;
    cleanup_abandoned_transaction(root, journal.transaction_id, tx)
}

fn cleanup_abandoned_transaction(root: &Path, transaction_id: Uuid, tx: &File) -> Result<()> {
    unlinkat_if_exists(tx, PROGRESS_NAME, 0)?;
    tx.sync_all()?;
    let control = open_directory(&root.join(".wheat"))?;
    let transactions = open_dir_at(&control, TRANSACTIONS)?;
    unlinkat_if_exists(
        &transactions,
        &transaction_id.to_string(),
        libc::AT_REMOVEDIR,
    )?;
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
    let duplicate = unsafe { libc::dup(transactions.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error()).context("duplicate transactions directory handle");
    }
    let path = format!("/proc/self/fd/{duplicate}");
    let entries = fs::read_dir(path)?
        .take(MAX_ORPHAN_TRANSACTIONS + 1)
        .collect::<io::Result<Vec<_>>>();
    unsafe { libc::close(duplicate) };
    let entries = entries?;
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
                let progress = open_progress(&tx)?;
                ensure!(
                    progress.metadata()?.len() == 0,
                    "orphan materialization transaction has recorded intent"
                );
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
) -> Result<()> {
    ensure!(
        !replacement.expected_leaves.is_empty(),
        "replacement directory has no tracked descendants"
    );
    inventory_directory(root, path, replacement)
}

fn inventory_directory(root: &Path, path: &str, expected: &ReplacedDirectoryPlan) -> Result<()> {
    for entry in fs::read_dir(root.join(path)).context("inventory replacement directory")? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 entry in replacement directory"))?;
        let relative = format!("{path}/{name}");
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            ensure!(
                expected.expected_directories.contains(relative.as_str()),
                "refusing to replace workspace directory {path:?} containing untracked entry {relative:?}"
            );
            inventory_directory(root, &relative, expected)?;
        } else {
            ensure!(
                expected.expected_leaves.contains(relative.as_str()),
                "refusing to replace workspace directory {path:?} containing untracked entry {relative:?}"
            );
        }
    }
    Ok(())
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
    let name = c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error.into())
        };
    }
    Ok(unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR)
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
    match unlinkat_if_exists(&parent, name, libc::AT_REMOVEDIR) {
        Ok(()) => parent.sync_all()?,
        Err(error)
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                )
            }) =>
        {
            bail!(
                "refusing to replace nonempty workspace directory {:?}",
                install.path
            )
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
        match unlinkat_if_exists(&directory, name, libc::AT_REMOVEDIR) {
            Ok(()) => directory.sync_all()?,
            Err(error)
                if error.downcast_ref::<io::Error>().is_some_and(|error| {
                    matches!(
                        error.raw_os_error(),
                        Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                    )
                }) =>
            {
                break;
            }
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

#[cfg(target_os = "linux")]
fn rename_noreplace(
    source: &File,
    source_name: &str,
    destination: &File,
    destination_name: &str,
) -> Result<()> {
    let source_name = c_string(source_name)?;
    let destination_name = c_string(destination_name)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.as_raw_fd(),
            source_name.as_ptr(),
            destination.as_raw_fd(),
            destination_name.as_ptr(),
            1_u32,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("install without replacement");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(
    _source: &File,
    _source_name: &str,
    _destination: &File,
    _destination_name: &str,
) -> Result<()> {
    bail!("Wheat workspace materialization is currently supported only on Linux")
}

#[cfg(target_os = "linux")]
fn rename_exchange(
    source: &File,
    source_name: &str,
    destination: &File,
    destination_name: &str,
) -> Result<()> {
    let source_name = c_string(source_name)?;
    let destination_name = c_string(destination_name)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.as_raw_fd(),
            source_name.as_ptr(),
            destination.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("exchange workspace leaf");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_exchange(
    _source: &File,
    _source_name: &str,
    _destination: &File,
    _destination_name: &str,
) -> Result<()> {
    bail!("Wheat workspace materialization is currently supported only on Linux")
}

fn open_directory(path: &Path) -> Result<File> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .context("open directory without following symlinks");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_dir_at(directory: &File, name: &str) -> Result<File> {
    open_dir_at_optional(directory, name)?.context("required directory is missing")
}

fn open_dir_at_optional(directory: &File, name: &str) -> Result<Option<File>> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("open child directory without following symlinks");
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn mkdirat_new(directory: &File, name: &str, mode: libc::mode_t) -> Result<()> {
    let name = c_string(name)?;
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error()).context("create workspace directory");
    }
    Ok(())
}

fn mkdirat_if_missing(directory: &File, name: &str, mode: libc::mode_t) -> Result<()> {
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

fn create_regular_at(directory: &File, name: &str, mode: libc::mode_t) -> Result<File> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("create private materialization file");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
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
            let name = c_string(name)?;
            let target = CString::new(bytes)?;
            if unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), name.as_ptr()) }
                != 0
            {
                return Err(io::Error::last_os_error()).context("stage symlink");
            }
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
    let name = c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("stat staged materialization leaf");
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
        if kind != FileKind::Symlink || expected.len() > 4_095 {
            return Ok(false);
        }
        let mut bytes = [0_u8; 4_096];
        let read = unsafe {
            libc::readlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error()).context("read staged symlink");
        }
        return Ok(&bytes[..usize::try_from(read)?] == expected);
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || kind == FileKind::Symlink {
        return Ok(false);
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open staged file");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected.len() as u64 {
        return Ok(false);
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    let actual_kind = if metadata.mode() & 0o111 != 0 {
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
    let name = c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("stat staged materialization leaf");
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
        let mut bytes = vec![0_u8; limit.saturating_add(1)];
        let read = unsafe {
            libc::readlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error()).context("read staged symlink");
        }
        let read = usize::try_from(read)?;
        ensure!(read <= limit, "materialized symlink exceeds byte limit");
        bytes.truncate(read);
        return Ok(WorkspaceFileSnapshot {
            kind: FileKind::Symlink,
            bytes,
        });
    }
    ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFREG,
        "staged target is not a regular file or symlink"
    );
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open staged file");
    }
    let file = unsafe { File::from_raw_fd(fd) };
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
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    let kind = if metadata.mode() & 0o111 != 0 {
        FileKind::Executable
    } else {
        FileKind::Regular
    };
    Ok(WorkspaceFileSnapshot { kind, bytes })
}

fn leaf_exists(directory: &File, name: &str) -> Result<bool> {
    let name = c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error.into())
    }
}

enum OrphanTransactionShape {
    Empty,
    EmptyProgress,
}

fn orphan_transaction_shape(directory: &File) -> Result<OrphanTransactionShape> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error()).context("duplicate transaction directory handle");
    }
    let path = format!("/proc/self/fd/{duplicate}");
    let result = (|| {
        let mut entries = fs::read_dir(path)?;
        let first = entries.next().transpose()?;
        let second = entries.next().transpose()?;
        match (first, second) {
            (None, None) => Ok(OrphanTransactionShape::Empty),
            (Some(entry), None) if entry.file_name() == PROGRESS_NAME => {
                Ok(OrphanTransactionShape::EmptyProgress)
            }
            _ => bail!("materialization transaction contains unexpected debris"),
        }
    })();
    unsafe { libc::close(duplicate) };
    result
}

fn ensure_progress_only(directory: &File) -> Result<()> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error()).context("duplicate transaction directory handle");
    }
    let path = format!("/proc/self/fd/{duplicate}");
    let mut entries = fs::read_dir(path)?;
    let only = entries
        .next()
        .transpose()?
        .is_some_and(|entry| entry.file_name() == PROGRESS_NAME)
        && entries.next().is_none();
    unsafe { libc::close(duplicate) };
    ensure!(
        only,
        "materialization transaction contains unexpected debris"
    );
    Ok(())
}

fn unlinkat_if_exists(directory: &File, name: &str, flags: libc::c_int) -> Result<()> {
    let name = c_string(name)?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
}

fn c_string(value: &str) -> Result<CString> {
    Ok(CString::new(value.as_bytes())?)
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
        Ok("exit") => unsafe { libc::_exit(86) },
        Ok("sigkill") => unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
            libc::_exit(87);
        },
        Ok(other) => bail!("invalid checkpoint failure mode {other:?}"),
        Err(std::env::VarError::NotPresent) => {
            bail!("injected materialization interruption after {name}")
        }
        Err(error) => bail!("read checkpoint failure mode: {error}"),
    }
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

    #[test]
    fn progress_log_is_fixed_width_and_recovers_a_torn_tail() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        append_install_intent(&mut progress, id, 0).unwrap();
        append_install_intent(&mut progress, id, 1).unwrap();
        progress.write_all(b"torn").unwrap();
        progress.sync_data().unwrap();

        assert_eq!(read_progress(&mut progress, id, 3).unwrap(), 2);
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
        append_install_intent(&mut progress, id, 0).unwrap();
        append_install_intent(&mut progress, id, 1).unwrap();
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

        assert_eq!(read_progress(&mut progress, id, 3).unwrap(), 1);
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
                staged: String::new(),
                tombstone: Some(Vec::new()),
            });
        }
        for index in 0..REPLACEMENTS {
            installs.push(Install {
                path: format!("root-{index:05}"),
                old: None,
                new: Some(value),
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
                staged: String::new(),
                tombstone: Some(Vec::new()),
            },
            Install {
                path: String::from("root"),
                old: None,
                new: Some(value),
                staged: String::new(),
                tombstone: None,
            },
            Install {
                path: nearest_replacement.clone(),
                old: None,
                new: Some(value),
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
    fn progress_log_rejects_corrupt_interior_records() {
        let root = tempfile::tempdir().unwrap();
        let tx = open_directory(root.path()).unwrap();
        let id = Uuid::now_v7();
        let mut progress = create_progress(&tx).unwrap();
        for index in 0..3 {
            append_install_intent(&mut progress, id, index).unwrap();
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
