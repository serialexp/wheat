//! Crash-recoverable first materialization of a separate-store workspace.
//!
//! Initial materialization is monotonic: targets are staged durably and installed
//! with `RENAME_NOREPLACE`. Recovery only verifies and rolls forward. It never
//! removes or overwrites a workspace path. Updating an existing materialization
//! requires a separate preimage transaction and is deliberately refused here.

use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wheat_core::{FileKind, FileValue, TreeId, ViewId};
use wheat_store::{MerkleTreeStore, PublicationStore, RepositoryHead, TreeFile, TreeWalkLimits};

use crate::{
    local::LocalRepository,
    workspace_files::{WorkspaceFileSnapshot, read_workspace_file},
    workspace_metadata::{
        MaterializedState, WorkspaceMetadata, WorkspaceMetadataPublishError, WorkspaceSelection,
    },
};

const VERSION: u32 = 1;
const JOURNAL_NAME: &str = "materialization.json";
const TRANSACTIONS: &str = "materialization-transactions";
const PROGRESS_NAME: &str = "progress.log";
const PROGRESS_MAGIC: &[u8; 4] = b"WMI1";
const PROGRESS_RECORD_BYTES: usize = 36;
const MAX_JOURNAL_BYTES: usize = 192 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TEST_CHECKPOINT_ENV: &str = "WHEAT_TEST_MATERIALIZATION_FAIL_AFTER";
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
    value: FileValue,
    staged: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    transaction_id: Uuid,
    workspace_id: Uuid,
    store_id: Uuid,
    selection: WorkspaceSelection,
    target: MaterializedState,
    installs: Vec<Install>,
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
    if let Some(current) = &metadata.materialized {
        ensure!(
            current == &target && metadata.selection == requested_selection,
            "updating an existing materialized workspace is not implemented; create a fresh workspace"
        );
        ensure!(
            repository.heads.read()? == head,
            "repository head changed while materialization was resolved"
        );
        return Ok(());
    }

    let files = target_files(repository, tree).await?;
    validate_materializable(repository, &files)?;
    validate_initial_workspace(root, &files)?;
    ensure!(
        repository.heads.read()? == head,
        "repository head changed while materialization was planned"
    );

    let transaction_id = Uuid::now_v7();
    let journal = Journal {
        version: VERSION,
        transaction_id,
        workspace_id: metadata.workspace_id,
        store_id: metadata.store.store_id,
        selection: requested_selection,
        target,
        installs: files
            .into_iter()
            .enumerate()
            .map(|(index, file)| Install {
                path: file.path,
                value: file.value,
                staged: format!("new-{index:08x}"),
            })
            .collect(),
    };
    let journal_bytes = encode_journal(&journal)?;
    let (tx, mut progress) = create_transaction(root, transaction_id)?;
    if let Err(error) = write_journal(root, &journal_bytes) {
        if !journal_exists(root)? {
            cleanup_abandoned_transaction(root, transaction_id, &tx)?;
        }
        return Err(error);
    }
    checkpoint("journal-created")?;
    roll_forward(root, repository, &tx, &mut progress, &journal)?;
    checkpoint("all-installed")?;
    publish_target(root, metadata, &journal)?;
    cleanup_committed(root, transaction_id, &tx)?;
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
        return Ok(());
    };
    authenticate_journal(repository, metadata, &journal).await?;
    let (tx, mut progress) = open_transaction(root, journal.transaction_id)?;
    if metadata.materialized.as_ref() == Some(&journal.target) {
        verify_installed(root, repository, &journal)?;
        return cleanup_committed(root, journal.transaction_id, &tx);
    }
    ensure!(
        metadata.materialized.is_none(),
        "materialization journal conflicts with workspace metadata"
    );
    roll_forward(root, repository, &tx, &mut progress, &journal)?;
    publish_target(root, metadata, &journal)?;
    cleanup_committed(root, journal.transaction_id, &tx)
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
            && (metadata.materialized.is_none() || journal.selection == metadata.selection),
        "materialization journal does not belong to this workspace/store state"
    );
    authenticate_target(repository, journal).await?;
    let files = target_files(repository, journal.target.tree).await?;
    ensure!(
        files.len() == journal.installs.len(),
        "journal does not cover target tree"
    );
    for (index, (file, install)) in files.iter().zip(&journal.installs).enumerate() {
        ensure!(
            file.path == install.path
                && file.value == install.value
                && install.staged == format!("new-{index:08x}"),
            "journal does not match authenticated target tree"
        );
    }
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

fn roll_forward(
    root: &Path,
    repository: &LocalRepository,
    tx: &File,
    progress: &mut File,
    journal: &Journal,
) -> Result<()> {
    let completed = read_progress(progress, journal.transaction_id, journal.installs.len())?;
    let mut total = 0_u64;
    for (index, install) in journal.installs.iter().enumerate() {
        let expected = repository.content.get(install.value.content)?;
        ensure!(
            expected.len() <= MAX_FILE_BYTES,
            "materialized file exceeds byte limit"
        );
        total = total
            .checked_add(u64::try_from(expected.len())?)
            .context("materialization byte count overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES,
            "materialization exceeds total byte limit"
        );
        let has_intent = index < completed;
        let staged = leaf_exists(tx, &install.staged)?;
        let installed = read_workspace_file(root, Path::new(&install.path), MAX_FILE_BYTES)?;
        match (has_intent, staged, installed) {
            (false, _, Some(_)) => {
                bail!(
                    "workspace path appeared before install intent: {:?}",
                    install.path
                )
            }
            (true, true, Some(_)) | (true, false, None) => {
                bail!("ambiguous interrupted install for {:?}", install.path)
            }
            (true, false, Some(actual)) => {
                authenticate_snapshot_bytes(install.value, &expected, &actual)?;
                let (parent, _) = open_parent(root, &install.path)?;
                parent.sync_all()?;
                tx.sync_all()?;
                continue;
            }
            (_, true, None) => {
                if authenticate_exact_at(tx, &install.staged, repository, install.value).is_err() {
                    // A crash can leave a partial regular file in the private transaction
                    // directory. Before an install intent it was never eligible for installation,
                    // so discard only that private debris and recreate authenticated store bytes.
                    ensure!(!has_intent, "staged content changed after install intent");
                    unlinkat_if_exists(tx, &install.staged, 0)?;
                    tx.sync_all()?;
                    stage_install(tx, install, &expected)?;
                }
            }
            (false, false, None) => stage_install(tx, install, &expected)?,
        }

        checkpoint("staged")?;
        ensure_parent_directories(root, &install.path)?;
        if !has_intent {
            append_install_intent(progress, journal.transaction_id, index)?;
            checkpoint("intent-persisted")?;
        }
        let (parent, name) = open_parent(root, &install.path)?;
        rename_noreplace(tx, &install.staged, &parent, name)?;
        parent.sync_all()?;
        tx.sync_all()?;
        checkpoint("installed")?;
        verify_install(root, install, &expected)?;
    }
    Ok(())
}

fn stage_install(tx: &File, install: &Install, bytes: &[u8]) -> Result<()> {
    create_leaf_at(tx, &install.staged, install.value.kind, bytes)?;
    tx.sync_all()?;
    Ok(())
}

fn verify_install(root: &Path, install: &Install, expected: &[u8]) -> Result<()> {
    let actual = read_workspace_file(root, Path::new(&install.path), MAX_FILE_BYTES)?
        .with_context(|| format!("installed target is missing: {:?}", install.path))?;
    authenticate_snapshot_bytes(install.value, expected, &actual)
}

fn verify_installed(root: &Path, repository: &LocalRepository, journal: &Journal) -> Result<()> {
    for install in &journal.installs {
        let actual = read_workspace_file(root, Path::new(&install.path), MAX_FILE_BYTES)?
            .with_context(|| format!("committed target is missing: {:?}", install.path))?;
        authenticate_snapshot_exact(repository, install.value, &actual)?;
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

fn create_transaction(root: &Path, id: Uuid) -> Result<(File, File)> {
    let control = open_directory(&root.join(".wheat"))?;
    mkdirat_if_missing(&control, TRANSACTIONS, 0o700)?;
    let transactions = open_dir_at(&control, TRANSACTIONS)?;
    let name = id.to_string();
    mkdirat_new(&transactions, &name, 0o700)?;
    transactions.sync_all()?;
    let tx = open_dir_at(&transactions, &name)?;
    let progress = create_progress(&tx)?;
    tx.sync_all()?;
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
    for index in 0..(complete_bytes / PROGRESS_RECORD_BYTES) {
        progress.read_exact(&mut record)?;
        validate_progress_record(&record, transaction_id, index)
            .context("materialization progress contains an invalid complete record")?;
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
        rename_replace(&control, &temporary_name, &control, JOURNAL_NAME)?;
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
fn cleanup_committed(root: &Path, transaction_id: Uuid, tx: &File) -> Result<()> {
    ensure_progress_only(tx)?;
    let control = open_directory(&root.join(".wheat"))?;
    unlinkat_if_exists(&control, JOURNAL_NAME, 0)?;
    control.sync_all()?;
    cleanup_abandoned_transaction(root, transaction_id, tx)
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

fn rename_replace(
    source: &File,
    source_name: &str,
    destination: &File,
    destination_name: &str,
) -> Result<()> {
    let source_name = c_string(source_name)?;
    let destination_name = c_string(destination_name)?;
    if unsafe {
        libc::renameat(
            source.as_raw_fd(),
            source_name.as_ptr(),
            destination.as_raw_fd(),
            destination_name.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("publish materialization journal");
    }
    Ok(())
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
    if std::env::var_os(TEST_CHECKPOINT_ENV).as_deref() == Some(std::ffi::OsStr::new(name)) {
        bail!("injected materialization interruption after {name}");
    }
    Ok(())
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
