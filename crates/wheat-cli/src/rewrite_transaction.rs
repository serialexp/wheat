//! Recoverable store-scoped metadata transactions for history rewrites.
//!
//! Graph planning is intentionally outside this module.  A planner supplies the exact old and
//! new workspace metadata plus the two repository heads.  This layer authenticates and locks the
//! registry snapshot, durably records both sides, and makes recovery an all-old/all-new decision.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wheat_store::{PublicationStore, RepositoryHead};

use crate::{
    local::LocalRepository,
    materialization::WorkspaceLock,
    workspace_metadata::{SerializedHead, WorkspaceMetadata},
    workspace_registry::{WorkspacePin, WorkspaceRegistry},
    workspace_registry_service,
};

const FORMAT: &str = "wheat-store-rewrite-transaction";
const VERSION: u32 = 1;
const TRANSACTION_FILE: &str = "rewrite-transaction.json";
const LOCK_FILE: &str = "rewrite.lock";
const MAX_WORKSPACES: usize = 4096;
const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceRewrite {
    pub root: PathBuf,
    pub old: WorkspaceMetadata,
    pub new: WorkspaceMetadata,
}

#[derive(Clone, Debug)]
pub(crate) struct RewritePlan {
    pub expected_head: RepositoryHead,
    pub proposed_head: RepositoryHead,
    pub workspaces: Vec<WorkspaceRewrite>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    format: String,
    version: u32,
    store_id: Uuid,
    expected_head: SerializedHead,
    proposed_head: SerializedHead,
    workspaces: Vec<TransactionWorkspace>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionWorkspace {
    root: PathBuf,
    old: WorkspaceMetadata,
    new: WorkspaceMetadata,
    old_pin: WorkspacePin,
    new_pin: WorkspacePin,
}

#[cfg(test)]
fn begin<'a>(store_root: &'a Path, plan: RewritePlan) -> Result<RewriteGuard<'a>> {
    let guard = RewriteGuard::acquire(store_root)?;
    stage_with_guard(store_root, plan, &guard)?;
    Ok(guard)
}

#[cfg(test)]
fn recover(store_root: &Path, repository: &LocalRepository) -> Result<()> {
    let _operation = StoreOperationGuard::acquire(store_root, repository)?;
    Ok(())
}

fn stage_with_guard(store_root: &Path, plan: RewritePlan, guard: &RewriteGuard<'_>) -> Result<()> {
    ensure!(
        !transaction_path(store_root).exists(),
        "a store rewrite transaction already requires recovery"
    );
    let store = crate::workspace_metadata::StoreMetadata::read(store_root)?;
    ensure!(
        plan.expected_head.generation < plan.proposed_head.generation,
        "rewrite proposed head must advance the repository generation"
    );
    ensure!(
        !plan.workspaces.is_empty() && plan.workspaces.len() <= MAX_WORKSPACES,
        "rewrite must affect between 1 and {MAX_WORKSPACES} workspaces"
    );

    let registry = WorkspaceRegistry::new(store_root, store.store_id)?;
    let pins = authenticated_registry(&registry, store_root, store.store_id)?;
    let requested: BTreeSet<_> = plan
        .workspaces
        .iter()
        .map(|item| item.old.workspace_id)
        .collect();
    ensure!(
        requested.len() == plan.workspaces.len(),
        "duplicate workspace in rewrite plan"
    );
    let registered: BTreeSet<_> = pins.iter().map(|pin| pin.workspace_id).collect();
    ensure!(
        requested == registered,
        "rewrite plan must cover every registered workspace"
    );

    let mut workspaces = plan.workspaces;
    workspaces.sort_by(|left, right| left.root.as_os_str().cmp(right.root.as_os_str()));
    for item in &workspaces {
        authenticate_pair(item, store_root, store.store_id)?;
        let pin = pins
            .iter()
            .find(|pin| pin.workspace_id == item.old.workspace_id)
            .context("rewrite workspace is absent from authenticated registry")?;
        ensure!(
            item.root.canonicalize()? == pin.workspace_path,
            "rewrite workspace path does not match its registration"
        );
    }
    guard.lock_workspaces(&workspaces)?;
    // Re-read after locking to close the authentication/lock race.
    for item in &workspaces {
        authenticate_current(item)?;
    }

    let transaction = Transaction {
        format: FORMAT.into(),
        version: VERSION,
        store_id: store.store_id,
        expected_head: plan.expected_head.into(),
        proposed_head: plan.proposed_head.into(),
        workspaces: workspaces
            .into_iter()
            .map(|item| {
                let old_pin =
                    workspace_registry_service::pin_for(&item.root, store_root, &item.old)?;
                let new_pin =
                    workspace_registry_service::pin_for(&item.root, store_root, &item.new)?;
                Ok(TransactionWorkspace {
                    root: item.root,
                    old: item.old,
                    new: item.new,
                    old_pin,
                    new_pin,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    write_transaction(store_root, &transaction)?;
    Ok(())
}

/// Store-wide admission guard for every operation whose result depends on the repository head.
///
/// Lock order is always the store operation lock first, then workspace locks in canonical path
/// order. Callers that also need their current workspace lock must acquire it only after this
/// guard. Recovery temporarily takes every workspace lock named by a staged rewrite, chooses the
/// old or new side from the repository head, and releases those workspace locks before returning.
/// The store lock remains held, so no unrelated operation can observe the staged transaction and
/// advance the head between recovery/planning and its compare-and-swap.
pub(crate) struct StoreOperationGuard<'a> {
    _rewrite: RewriteGuard<'a>,
}

impl<'a> StoreOperationGuard<'a> {
    #[tracing::instrument(name = "store_lock", level = "debug", skip_all)]
    pub(crate) fn acquire(store_root: &'a Path, repository: &LocalRepository) -> Result<Self> {
        let rewrite = RewriteGuard::acquire(store_root)?;
        if let Some(transaction) = read_transaction(store_root)? {
            rewrite.lock_transaction_workspaces(&transaction)?;
            recover_transaction(store_root, repository, &transaction)?;
            rewrite.workspace_locks.borrow_mut().clear();
        }
        Ok(Self { _rewrite: rewrite })
    }

    /// Stage a rewrite while retaining the same store admission held during planning.
    pub(crate) fn stage(&self, store_root: &Path, plan: RewritePlan) -> Result<()> {
        ensure!(
            self._rewrite.store_root == store_root,
            "store operation guard belongs to another store"
        );
        stage_with_guard(store_root, plan, &self._rewrite)
    }

    /// Resolve the transaction staged through this guard after its repository CAS.
    pub(crate) fn recover_staged(
        &self,
        store_root: &Path,
        repository: &LocalRepository,
    ) -> Result<()> {
        recover_locked(store_root, repository, &self._rewrite)
    }
}

/// Recover while retaining the guard that staged the transaction.
pub(crate) fn recover_locked(
    store_root: &Path,
    repository: &LocalRepository,
    guard: &RewriteGuard<'_>,
) -> Result<()> {
    let transaction = read_transaction(store_root)?.context("rewrite transaction is not staged")?;
    ensure!(
        transaction.store_id == repository.store_id,
        "rewrite transaction store identity mismatch"
    );
    ensure!(
        guard.store_root == store_root,
        "rewrite guard belongs to another store"
    );
    if guard.workspace_locks.borrow().len() != transaction.workspaces.len() {
        guard.lock_transaction_workspaces(&transaction)?;
    }
    recover_transaction(store_root, repository, &transaction)
}

fn recover_transaction(
    store_root: &Path,
    repository: &LocalRepository,
    transaction: &Transaction,
) -> Result<()> {
    let head = repository.heads.read()?;
    let expected: RepositoryHead = transaction.expected_head.into();
    let proposed: RepositoryHead = transaction.proposed_head.into();
    let use_new = if head == expected {
        false
    } else if head == proposed {
        true
    } else {
        bail!(
            "rewrite transaction cannot recover at unrelated repository head generation {}",
            head.generation
        );
    };
    apply_side(store_root, transaction, use_new, |_| Ok(()))
}

fn apply_side(
    store_root: &Path,
    transaction: &Transaction,
    use_new: bool,
    mut checkpoint: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    let registry = WorkspaceRegistry::new(store_root, transaction.store_id)?;
    for (index, item) in transaction.workspaces.iter().enumerate() {
        let (metadata, pin) = if use_new {
            (&item.new, &item.new_pin)
        } else {
            (&item.old, &item.old_pin)
        };
        metadata
            .replace_prepared(&item.root)
            .with_context(|| format!("prepare rewrite metadata for {}", item.root.display()))?
            .publish()
            .map_err(anyhow::Error::new)?;
        registry.publish(pin)?;
        checkpoint(index + 1)?;
    }
    fs::remove_file(transaction_path(store_root))
        .context("remove completed rewrite transaction")?;
    File::open(store_root)?.sync_all()?;
    Ok(())
}

pub(crate) fn preflight_workspaces(store_root: &Path) -> Result<Vec<(PathBuf, WorkspaceMetadata)>> {
    let store = crate::workspace_metadata::StoreMetadata::read(store_root)?;
    let registry = WorkspaceRegistry::new(store_root, store.store_id)?;
    authenticated_registry(&registry, store_root, store.store_id)?
        .into_iter()
        .map(|pin| {
            let metadata = WorkspaceMetadata::read(&pin.workspace_path)?;
            authenticate(&metadata, &pin, store_root, store.store_id)?;
            Ok((pin.workspace_path, metadata))
        })
        .collect()
}

fn authenticated_registry(
    registry: &WorkspaceRegistry,
    store_root: &Path,
    store_id: Uuid,
) -> Result<Vec<WorkspacePin>> {
    let mut retained = Vec::new();
    for pin in registry.enumerate()? {
        match fs::symlink_metadata(&pin.workspace_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                registry.remove(pin.workspace_id, pin.registration_token)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect registered workspace {}",
                        pin.workspace_path.display()
                    )
                });
            }
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "registered workspace path is a symbolic link: {}",
                    pin.workspace_path.display()
                );
                ensure!(
                    metadata.is_dir(),
                    "registered workspace path is not a directory: {}",
                    pin.workspace_path.display()
                );
                let current = WorkspaceMetadata::read(&pin.workspace_path)?;
                authenticate(&current, &pin, store_root, store_id)?;
                retained.push(pin);
            }
        }
    }
    Ok(retained)
}

fn authenticate(
    metadata: &WorkspaceMetadata,
    pin: &WorkspacePin,
    store_root: &Path,
    store_id: Uuid,
) -> Result<()> {
    ensure!(
        metadata.workspace_id == pin.workspace_id,
        "registered workspace identity mismatch"
    );
    ensure!(
        metadata.registration_token == pin.registration_token,
        "registered workspace token mismatch"
    );
    ensure!(
        metadata.store.store_id == store_id,
        "registered workspace store identity mismatch"
    );
    ensure!(
        metadata.store.path == store_root.canonicalize()?,
        "registered workspace store path mismatch"
    );
    Ok(())
}

fn authenticate_pair(item: &WorkspaceRewrite, store_root: &Path, store_id: Uuid) -> Result<()> {
    ensure!(
        item.root.is_absolute(),
        "rewrite workspace path must be absolute"
    );
    ensure!(
        item.old.workspace_id == item.new.workspace_id,
        "rewrite changes workspace identity"
    );
    ensure!(
        item.old.registration_token == item.new.registration_token,
        "rewrite changes registration token"
    );
    ensure!(
        item.old.store == item.new.store,
        "rewrite changes workspace store"
    );
    ensure!(
        item.old.store.store_id == store_id,
        "rewrite workspace belongs to another store"
    );
    ensure!(
        item.old.store.path == store_root.canonicalize()?,
        "rewrite workspace store path mismatch"
    );
    Ok(())
}

fn authenticate_current(item: &WorkspaceRewrite) -> Result<()> {
    let current = WorkspaceMetadata::read(&item.root)?;
    ensure!(
        current == item.old || current == item.new,
        "workspace metadata changed before rewrite staging"
    );
    Ok(())
}

fn transaction_path(store_root: &Path) -> PathBuf {
    store_root.join(TRANSACTION_FILE)
}

fn write_transaction(store_root: &Path, transaction: &Transaction) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(transaction)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "rewrite transaction exceeds the {MAX_DOCUMENT_BYTES}-byte limit"
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(".rewrite-")
        .suffix(".tmp")
        .tempfile_in(store_root)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(transaction_path(store_root))
        .map_err(|error| error.error)?;
    File::open(store_root)?.sync_all()?;
    Ok(())
}

pub(crate) fn pinned_roots(
    store_root: &Path,
) -> Result<crate::workspace_registry::WorkspaceRootSet> {
    let mut roots = crate::workspace_registry::WorkspaceRootSet::default();
    if let Some(transaction) = read_transaction(store_root)? {
        // The transaction itself is the union pin. Retain both repository authorities even when
        // no workspace happens to have a materialized head on either side.
        roots
            .repository_envelopes
            .insert(transaction.expected_head.envelope);
        roots
            .repository_envelopes
            .insert(transaction.proposed_head.envelope);
        for workspace in transaction.workspaces {
            roots.union_with(workspace.old_pin.roots());
            roots.union_with(workspace.new_pin.roots());
        }
    }
    Ok(roots)
}

fn read_transaction(store_root: &Path) -> Result<Option<Transaction>> {
    let path = transaction_path(store_root);
    let file = match open_read_no_follow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_DOCUMENT_BYTES,
        "invalid rewrite transaction file"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "rewrite transaction is too large"
    );
    let transaction: Transaction = serde_json::from_slice(&bytes)?;
    ensure!(
        transaction.format == FORMAT && transaction.version == VERSION,
        "unsupported rewrite transaction format"
    );
    ensure!(
        !transaction.store_id.is_nil(),
        "rewrite transaction has nil store identity"
    );
    ensure!(
        !transaction.workspaces.is_empty() && transaction.workspaces.len() <= MAX_WORKSPACES,
        "invalid rewrite workspace count"
    );
    Ok(Some(transaction))
}

pub(crate) struct RewriteGuard<'a> {
    store_root: &'a Path,
    _store_lock: File,
    workspace_locks: std::cell::RefCell<Vec<WorkspaceLock>>,
}

impl<'a> RewriteGuard<'a> {
    fn acquire(store_root: &'a Path) -> Result<Self> {
        let lock_path = store_root.join(LOCK_FILE);
        let file = open_lock_no_follow(&lock_path)?;
        try_lock(&file).with_context(|| "store rewrite is locked by another operation")?;
        Ok(Self {
            store_root,
            _store_lock: file,
            workspace_locks: std::cell::RefCell::new(Vec::new()),
        })
    }

    fn lock_workspaces(&self, workspaces: &[WorkspaceRewrite]) -> Result<()> {
        let mut locks = self.workspace_locks.borrow_mut();
        for item in workspaces {
            locks.push(WorkspaceLock::acquire(&item.root)?);
        }
        Ok(())
    }

    fn lock_transaction_workspaces(&self, transaction: &Transaction) -> Result<()> {
        self.lock_transaction_workspaces_except(transaction, Path::new(""))
    }

    fn lock_transaction_workspaces_except(
        &self,
        transaction: &Transaction,
        already_locked: &Path,
    ) -> Result<()> {
        let mut roots: Vec<_> = transaction
            .workspaces
            .iter()
            .map(|item| item.root.as_path())
            .collect();
        roots.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
        let mut locks = self.workspace_locks.borrow_mut();
        if !locks.is_empty() {
            return Ok(());
        }
        for root in roots {
            let current = WorkspaceMetadata::read(root)?;
            let item = transaction
                .workspaces
                .iter()
                .find(|item| item.root == root)
                .expect("root came from transaction");
            ensure!(
                current.workspace_id == item.old.workspace_id
                    && current.registration_token == item.old.registration_token,
                "rewrite workspace identity mismatch during recovery"
            );
            if root != already_locked {
                locks.push(WorkspaceLock::acquire(root)?);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_lock_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_lock_no_follow(path: &Path) -> io::Result<File> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symbolic links are not permitted",
        ));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn open_read_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_read_no_follow(path: &Path) -> io::Result<File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symbolic links are not permitted",
        ));
    }
    File::open(path)
}

#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_core::{RepositoryEnvelopeId, TreeId, ViewId};

    fn view(byte: u8) -> ViewId {
        ViewId::from_digest([byte; 32])
    }

    fn setup() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        LocalRepository,
        WorkspaceMetadata,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let store_root = temporary.path().join("store");
        crate::local::initialize(&store_root).unwrap();
        let store = crate::workspace_metadata::StoreMetadata::create(&store_root).unwrap();
        let store_root = store_root.canonicalize().unwrap();
        let workspace_root = temporary.path().join("workspace");
        fs::create_dir(&workspace_root).unwrap();
        let metadata =
            WorkspaceMetadata::new(store_root.clone(), store.store_id, "main".into(), view(1));
        metadata.initialize(&workspace_root).unwrap();
        workspace_registry_service::reconcile(&workspace_root, &store_root, &metadata).unwrap();
        let workspace_root = workspace_root.canonicalize().unwrap();
        let repository = crate::local::open(&store_root).unwrap();
        (temporary, store_root, workspace_root, repository, metadata)
    }

    fn plan(repository: &LocalRepository, root: PathBuf, old: WorkspaceMetadata) -> RewritePlan {
        let expected = repository.heads.read().unwrap();
        let mut new = old.clone();
        new.basis = view(2);
        new.observed_label_binding = view(3);
        new.selected_frontier = Some(view(4));
        if let Some(materialized) = &mut new.materialized {
            materialized.view = view(5);
            materialized.tree = TreeId::from_digest([6; 32]);
        }
        RewritePlan {
            expected_head: expected,
            proposed_head: RepositoryHead {
                generation: expected.generation + 1,
                envelope: RepositoryEnvelopeId::from_digest([9; 32]),
            },
            workspaces: vec![WorkspaceRewrite { root, old, new }],
        }
    }

    #[test]
    fn recovery_chooses_old_or_new_exclusively_and_is_idempotent() {
        for advance in [false, true] {
            let (_temporary, store_root, workspace_root, repository, old) = setup();
            fs::write(workspace_root.join("payload"), b"untouched").unwrap();
            let plan = plan(&repository, workspace_root.clone(), old.clone());
            let proposed = plan.proposed_head;
            let new = plan.workspaces[0].new.clone();
            let guard = begin(&store_root, plan).unwrap();
            if advance {
                repository
                    .heads
                    .compare_and_advance(repository.heads.read().unwrap(), proposed)
                    .unwrap();
            }
            recover_locked(&store_root, &repository, &guard).unwrap();
            assert_eq!(
                WorkspaceMetadata::read(&workspace_root).unwrap(),
                if advance { new } else { old }
            );
            assert_eq!(
                fs::read(workspace_root.join("payload")).unwrap(),
                b"untouched"
            );
            drop(guard);
            recover(&store_root, &repository).unwrap();
        }
    }

    #[test]
    fn interrupted_metadata_application_recovers_every_workspace_to_one_side() {
        let (_temporary, store_root, workspace_root, repository, old) = setup();
        let second_root = workspace_root.parent().unwrap().join("workspace-2");
        fs::create_dir(&second_root).unwrap();
        let mut second = WorkspaceMetadata::new(
            store_root.clone(),
            repository.store_id,
            "main".into(),
            view(1),
        );
        second.initialize(&second_root).unwrap();
        workspace_registry_service::reconcile(&second_root, &store_root, &second).unwrap();
        let second_root = second_root.canonicalize().unwrap();
        let mut rewrite = plan(&repository, workspace_root.clone(), old);
        second.basis = view(1);
        let mut second_new = second.clone();
        second_new.basis = view(7);
        rewrite.workspaces.push(WorkspaceRewrite {
            root: second_root.clone(),
            old: second.clone(),
            new: second_new.clone(),
        });
        let proposed = rewrite.proposed_head;
        let first_new = rewrite.workspaces[0].new.clone();
        let guard = begin(&store_root, rewrite).unwrap();
        repository
            .heads
            .compare_and_advance(repository.heads.read().unwrap(), proposed)
            .unwrap();
        let transaction = read_transaction(&store_root).unwrap().unwrap();
        assert!(
            apply_side(&store_root, &transaction, true, |completed| {
                if completed == 1 {
                    bail!("simulated crash")
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        drop(guard);
        recover(&store_root, &repository).unwrap();
        assert_eq!(WorkspaceMetadata::read(&workspace_root).unwrap(), first_new);
        assert_eq!(WorkspaceMetadata::read(&second_root).unwrap(), second_new);
    }

    #[test]
    fn unrelated_head_fails_closed_without_changing_metadata() {
        let (_temporary, store_root, workspace_root, repository, old) = setup();
        let rewrite = plan(&repository, workspace_root.clone(), old.clone());
        let guard = begin(&store_root, rewrite).unwrap();
        let expected = repository.heads.read().unwrap();
        repository
            .heads
            .compare_and_advance(
                expected,
                RepositoryHead {
                    generation: expected.generation + 1,
                    envelope: RepositoryEnvelopeId::from_digest([8; 32]),
                },
            )
            .unwrap();
        assert!(recover_locked(&store_root, &repository, &guard).is_err());
        assert_eq!(WorkspaceMetadata::read(&workspace_root).unwrap(), old);
        assert!(transaction_path(&store_root).is_file());
    }

    #[test]
    fn ordinary_store_operation_recovers_each_staged_side_before_a_third_head() {
        for publish_rewrite in [false, true] {
            let (_temporary, store_root, workspace_root, repository, old) = setup();
            let rewrite = plan(&repository, workspace_root.clone(), old.clone());
            let proposed = rewrite.proposed_head;
            let new = rewrite.workspaces[0].new.clone();
            let guard = begin(&store_root, rewrite).unwrap();
            if publish_rewrite {
                repository
                    .heads
                    .compare_and_advance(repository.heads.read().unwrap(), proposed)
                    .unwrap();
            }
            drop(guard);

            // Label/resolve/fetch/publish enter through this same admission wrapper. Recovery must
            // finish before any of them can plan from or advance the repository authority.
            let operation = StoreOperationGuard::acquire(&store_root, &repository).unwrap();
            assert!(!transaction_path(&store_root).exists());
            assert_eq!(
                WorkspaceMetadata::read(&workspace_root).unwrap(),
                if publish_rewrite { new } else { old }
            );
            let recovered = repository.heads.read().unwrap();
            let third = RepositoryHead {
                generation: recovered.generation + 1,
                envelope: RepositoryEnvelopeId::from_digest([7; 32]),
            };
            repository
                .heads
                .compare_and_advance(recovered, third)
                .unwrap();
            drop(operation);
            assert_eq!(repository.heads.read().unwrap(), third);
        }
    }

    #[test]
    fn affected_workspace_lock_and_identity_mismatch_fail_before_staging() {
        let (_temporary, store_root, workspace_root, repository, old) = setup();
        let held = WorkspaceLock::acquire(&workspace_root).unwrap();
        assert!(
            begin(
                &store_root,
                plan(&repository, workspace_root.clone(), old.clone())
            )
            .is_err()
        );
        assert!(!transaction_path(&store_root).exists());
        drop(held);

        let mut mismatched = old.clone();
        mismatched.registration_token = Uuid::now_v7();
        mismatched
            .replace_prepared(&workspace_root)
            .unwrap()
            .publish()
            .unwrap();
        assert!(begin(&store_root, plan(&repository, workspace_root, mismatched)).is_err());
        assert!(!transaction_path(&store_root).exists());
    }

    #[test]
    fn staged_transaction_pins_both_complete_metadata_sides() {
        let (_temporary, store_root, workspace_root, repository, old) = setup();
        let rewrite = plan(&repository, workspace_root, old);
        let old_basis = rewrite.workspaces[0].old.basis;
        let new_basis = rewrite.workspaces[0].new.basis;
        let guard = begin(&store_root, rewrite).unwrap();
        let roots = pinned_roots(&store_root).unwrap();
        assert!(roots.views.contains(&old_basis));
        assert!(roots.views.contains(&new_basis));
        drop(guard);
    }

    #[test]
    fn strict_bounded_transaction_reader_rejects_unknown_fields_and_oversize() {
        let root = tempfile::tempdir().unwrap();
        fs::write(transaction_path(root.path()), br#"{"format":"wheat-store-rewrite-transaction","version":1,"store_id":"00000000-0000-0000-0000-000000000001","expected_head":{"generation":0,"envelope":"0000000000000000000000000000000000000000000000000000000000000000"},"proposed_head":{"generation":1,"envelope":"0000000000000000000000000000000000000000000000000000000000000000"},"workspaces":[],"extra":true}"#).unwrap();
        assert!(read_transaction(root.path()).is_err());
        fs::write(
            transaction_path(root.path()),
            vec![b'x'; MAX_DOCUMENT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(read_transaction(root.path()).is_err());
    }
}
