//! Durable, store-local workspace roots for conservative collection.
//!
//! Registry documents are deliberately independent of workspace metadata: a workspace can
//! disappear or become unreadable without silently removing its last durable storage roots.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wheat_core::{ChangeId, RepositoryEnvelopeId, TreeId, ViewId};

const REGISTRY_FORMAT: &str = "wheat-workspace-pin";
const REGISTRY_VERSION: u32 = 1;
const REGISTRY_DIRECTORY: &str = "workspaces";
const REGISTRY_LOCK: &str = "workspace-registry.lock";
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;
const MAX_REGISTRY_ENTRIES: usize = 4096;
const MAX_ROOTS_PER_KIND: usize = 64;

/// A repository head retained by a workspace, including the generation used for CAS.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryRoot {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
}

/// Roots describing the last stable state of a workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StableWorkspaceRoots {
    pub basis: ViewId,
    pub observed_label_binding: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub selected_change: Option<ChangeId>,
    pub materialized: Option<MaterializationRoots>,
}

/// Roots needed to preserve an already materialized or in-flight materialization state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializationRoots {
    pub repository_head: RepositoryRoot,
    pub view: ViewId,
    pub tree: TreeId,
}

/// Both sides of an in-flight repository compare-and-swap must remain live until recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRepositoryCasRoots {
    pub expected_head: RepositoryRoot,
    pub proposed_head: RepositoryRoot,
    pub resulting_frontier: ViewId,
}

/// Strict, versioned pin document stored at `workspaces/<workspace-id>.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspacePin {
    format: String,
    version: u32,
    pub store_id: Uuid,
    pub workspace_id: Uuid,
    pub registration_token: Uuid,
    pub workspace_path: PathBuf,
    pub stable: StableWorkspaceRoots,
    pub pending_materialization: Option<MaterializationRoots>,
    pub pending_repository_cas: Option<PendingRepositoryCasRoots>,
}

/// Typed union consumed by a future collector. Sets make repeated roots harmless and ordered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkspaceRootSet {
    pub repository_envelopes: BTreeSet<RepositoryEnvelopeId>,
    pub views: BTreeSet<ViewId>,
    pub trees: BTreeSet<TreeId>,
    pub changes: BTreeSet<ChangeId>,
}

/// Access to one store's registry. Every operation validates the store identity.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceRegistry {
    store_root: PathBuf,
    store_id: Uuid,
}

impl WorkspacePin {
    pub(crate) fn new(
        store_id: Uuid,
        workspace_id: Uuid,
        registration_token: Uuid,
        workspace_path: PathBuf,
        stable: StableWorkspaceRoots,
    ) -> Self {
        Self {
            format: REGISTRY_FORMAT.to_owned(),
            version: REGISTRY_VERSION,
            store_id,
            workspace_id,
            registration_token,
            workspace_path,
            stable,
            pending_materialization: None,
            pending_repository_cas: None,
        }
    }

    pub(crate) fn roots(&self) -> WorkspaceRootSet {
        let mut roots = WorkspaceRootSet::default();
        roots.views.insert(self.stable.basis);
        roots.views.insert(self.stable.observed_label_binding);
        roots.views.extend(self.stable.selected_frontier);
        roots.changes.extend(self.stable.selected_change);
        if let Some(materialized) = self.stable.materialized {
            roots.insert_materialization(materialized);
        }
        if let Some(materialization) = self.pending_materialization {
            roots.insert_materialization(materialization);
        }
        if let Some(cas) = self.pending_repository_cas {
            roots.insert_repository(cas.expected_head);
            roots.insert_repository(cas.proposed_head);
            roots.views.insert(cas.resulting_frontier);
        }
        roots
    }

    fn validate(&self, expected_store_id: Uuid, expected_workspace_id: Uuid) -> Result<()> {
        ensure!(
            self.format == REGISTRY_FORMAT && self.version == REGISTRY_VERSION,
            "unsupported Wheat workspace pin format {:?} version {}",
            self.format,
            self.version
        );
        ensure!(
            !self.store_id.is_nil(),
            "workspace pin has a nil store identity"
        );
        ensure!(
            self.store_id == expected_store_id,
            "workspace pin belongs to store {}, expected {}",
            self.store_id,
            expected_store_id
        );
        ensure!(
            !self.workspace_id.is_nil(),
            "workspace pin has a nil workspace identity"
        );
        ensure!(
            self.workspace_id == expected_workspace_id,
            "workspace pin identity {} does not match filename identity {}",
            self.workspace_id,
            expected_workspace_id
        );
        ensure!(
            !self.registration_token.is_nil(),
            "workspace pin has a nil registration token"
        );
        ensure!(
            self.workspace_path.is_absolute(),
            "workspace pin path must be absolute"
        );
        let roots = self.roots();
        ensure!(
            roots.repository_envelopes.len() <= MAX_ROOTS_PER_KIND,
            "workspace pin has too many repository roots"
        );
        ensure!(
            roots.views.len() <= MAX_ROOTS_PER_KIND,
            "workspace pin has too many view roots"
        );
        ensure!(
            roots.trees.len() <= MAX_ROOTS_PER_KIND,
            "workspace pin has too many tree roots"
        );
        ensure!(
            roots.changes.len() <= MAX_ROOTS_PER_KIND,
            "workspace pin has too many change roots"
        );
        Ok(())
    }
}

impl WorkspaceRootSet {
    #[allow(dead_code)] // Collector-facing API; GC consumes this in the next maintenance slice.
    pub(crate) fn union_with(&mut self, other: Self) {
        self.repository_envelopes.extend(other.repository_envelopes);
        self.views.extend(other.views);
        self.trees.extend(other.trees);
        self.changes.extend(other.changes);
    }

    fn insert_repository(&mut self, root: RepositoryRoot) {
        self.repository_envelopes.insert(root.envelope);
    }

    fn insert_materialization(&mut self, roots: MaterializationRoots) {
        self.insert_repository(roots.repository_head);
        self.views.insert(roots.view);
        self.trees.insert(roots.tree);
    }
}

impl WorkspaceRegistry {
    pub(crate) fn new(store_root: impl Into<PathBuf>, store_id: Uuid) -> Result<Self> {
        ensure!(
            !store_id.is_nil(),
            "workspace registry requires a non-nil store identity"
        );
        Ok(Self {
            store_root: store_root.into(),
            store_id,
        })
    }

    /// Atomically creates or replaces one workspace's pin while holding the store-local lock.
    pub(crate) fn publish(&self, pin: &WorkspacePin) -> Result<()> {
        pin.validate(self.store_id, pin.workspace_id)?;
        let _lock = self.lock()?;
        let directory = self.ensure_registry_directory()?;
        let destination = directory.join(document_name(pin.workspace_id));
        if destination.exists() {
            let existing = self.read_path(&destination, pin.workspace_id)?;
            ensure!(
                existing.registration_token == pin.registration_token,
                "workspace registration token does not match the existing pin"
            );
            if existing.workspace_path != pin.workspace_path
                && existing.workspace_path.exists()
                && workspace_identity_matches(&existing.workspace_path, &existing)?
            {
                ensure!(
                    false,
                    "workspace identity is already active at {}; copied workspaces must be cloned to receive a new identity",
                    existing.workspace_path.display()
                );
            }
        }
        let mut bytes = serde_json::to_vec_pretty(pin)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
            "workspace pin is too large"
        );

        let mut temporary = tempfile::Builder::new()
            .prefix(".workspace-pin-")
            .suffix(".tmp")
            .tempfile_in(&directory)
            .with_context(|| {
                format!(
                    "create workspace pin temporary file in {}",
                    directory.display()
                )
            })?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)
            .with_context(|| format!("publish workspace pin {}", destination.display()))?;
        open_directory_no_follow(&directory)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn read(&self, workspace_id: Uuid) -> Result<WorkspacePin> {
        ensure!(!workspace_id.is_nil(), "workspace identity must not be nil");
        let _lock = self.lock()?;
        let directory = self.registry_directory()?;
        self.read_path(&directory.join(document_name(workspace_id)), workspace_id)
    }

    pub(crate) fn remove(&self, workspace_id: Uuid, registration_token: Uuid) -> Result<()> {
        ensure!(!workspace_id.is_nil(), "workspace identity must not be nil");
        ensure!(
            !registration_token.is_nil(),
            "registration token must not be nil"
        );
        let _lock = self.lock()?;
        let directory = self.registry_directory()?;
        let path = directory.join(document_name(workspace_id));
        let pin = self.read_path(&path, workspace_id)?;
        pin.validate(self.store_id, workspace_id)?;
        ensure!(
            pin.registration_token == registration_token,
            "workspace registration changed before pin removal"
        );
        fs::remove_file(&path)
            .with_context(|| format!("remove workspace pin {}", path.display()))?;
        open_directory_no_follow(&directory)?.sync_all()?;
        Ok(())
    }

    /// Returns validated pins in workspace-id order, failing closed on unknown registry entries.
    pub(crate) fn enumerate(&self) -> Result<Vec<WorkspacePin>> {
        let _lock = self.lock()?;
        let directory = self.registry_directory()?;
        let mut entries = Vec::new();
        let mut scanned_entries = 0_usize;
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("enumerate workspace registry {}", directory.display()))?
        {
            scanned_entries = scanned_entries
                .checked_add(1)
                .context("workspace registry entry count overflowed")?;
            ensure!(
                scanned_entries <= MAX_REGISTRY_ENTRIES,
                "workspace registry exceeds the {MAX_REGISTRY_ENTRIES}-entry limit"
            );
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .context("workspace registry contains a non-UTF-8 filename")?;
            if name.starts_with(".workspace-pin-") && name.ends_with(".tmp") {
                continue;
            }
            let workspace_id = workspace_id_from_name(name)?;
            entries.push((workspace_id, entry.path()));
        }
        entries.sort_unstable_by_key(|(workspace_id, _)| *workspace_id);
        entries
            .into_iter()
            .map(|(workspace_id, path)| self.read_path(&path, workspace_id))
            .collect()
    }

    #[allow(dead_code)] // Collector-facing API; GC consumes this in the next maintenance slice.
    pub(crate) fn root_set(&self) -> Result<WorkspaceRootSet> {
        let mut roots = WorkspaceRootSet::default();
        for pin in self.enumerate()? {
            roots.union_with(pin.roots());
        }
        roots.union_with(crate::rewrite_transaction::pinned_roots(&self.store_root)?);
        Ok(roots)
    }

    fn read_path(&self, path: &Path, workspace_id: Uuid) -> Result<WorkspacePin> {
        let file = open_read_no_follow(path)
            .with_context(|| format!("open workspace pin {}", path.display()))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "workspace pin is not a regular file: {}",
            path.display()
        );
        ensure!(
            metadata.len() <= MAX_DOCUMENT_BYTES,
            "workspace pin exceeds the {MAX_DOCUMENT_BYTES}-byte limit: {}",
            path.display()
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
            "workspace pin exceeds the {MAX_DOCUMENT_BYTES}-byte limit: {}",
            path.display()
        );
        let pin: WorkspacePin = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode workspace pin {}", path.display()))?;
        pin.validate(self.store_id, workspace_id)?;
        Ok(pin)
    }

    fn lock(&self) -> Result<RegistryLock> {
        let path = self.store_root.join(REGISTRY_LOCK);
        let file = open_lock_no_follow(&path)
            .with_context(|| format!("open workspace registry lock {}", path.display()))?;
        lock_exclusive(&file)
            .with_context(|| format!("lock workspace registry {}", path.display()))?;
        Ok(RegistryLock(file))
    }

    fn ensure_registry_directory(&self) -> Result<PathBuf> {
        let path = self.store_root.join(REGISTRY_DIRECTORY);
        match fs::create_dir(&path) {
            Ok(()) => open_directory_no_follow(&self.store_root)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create workspace registry {}", path.display()));
            }
        }
        open_directory_no_follow(&path)
            .with_context(|| format!("validate workspace registry directory {}", path.display()))?;
        Ok(path)
    }

    fn registry_directory(&self) -> Result<PathBuf> {
        let path = self.store_root.join(REGISTRY_DIRECTORY);
        open_directory_no_follow(&path)
            .with_context(|| format!("open workspace registry directory {}", path.display()))?;
        Ok(path)
    }
}

struct RegistryLock(File);

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = unlock(&self.0);
    }
}

fn workspace_identity_matches(path: &Path, expected: &WorkspacePin) -> Result<bool> {
    let metadata_path = path.join(".wheat/workspace.json");
    let file = match open_read_no_follow(&metadata_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("open {}", metadata_path.display()));
        }
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "workspace metadata is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_DOCUMENT_BYTES,
        "workspace metadata is too large"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "workspace metadata is too large"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let workspace_id = value
        .get("workspace_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let registration_token = value
        .get("registration_token")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    Ok(workspace_id == Some(expected.workspace_id)
        && registration_token == Some(expected.registration_token))
}

fn document_name(workspace_id: Uuid) -> String {
    format!("{workspace_id}.json")
}

fn workspace_id_from_name(name: &str) -> Result<Uuid> {
    let stem = name
        .strip_suffix(".json")
        .context("workspace registry entry does not end in .json")?;
    let workspace_id =
        Uuid::parse_str(stem).context("workspace registry entry is not named by a UUID")?;
    ensure!(
        !workspace_id.is_nil(),
        "workspace registry entry has a nil identity"
    );
    ensure!(
        document_name(workspace_id) == name,
        "workspace registry filename is not canonical: {name}"
    );
    Ok(workspace_id)
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
    refuse_symlink(path, true)?;
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
    refuse_symlink(path, false)?;
    File::open(path)
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    refuse_symlink(path, false)?;
    let file = File::open(path)?;
    if file.metadata()?.is_dir() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ))
    }
}

#[cfg(not(unix))]
fn refuse_symlink(path: &Path, allow_missing: bool) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symbolic links are not permitted",
        )),
        Ok(_) => Ok(()),
        Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> io::Result<()> {
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn unlock(file: &File) -> io::Result<()> {
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn unlock(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> [u8; 32] {
        [byte; 32]
    }
    fn view(byte: u8) -> ViewId {
        ViewId::from_digest(digest(byte))
    }
    fn tree(byte: u8) -> TreeId {
        TreeId::from_digest(digest(byte))
    }
    fn envelope(byte: u8) -> RepositoryEnvelopeId {
        RepositoryEnvelopeId::from_digest(digest(byte))
    }

    fn pin(store_id: Uuid, workspace_id: Uuid, byte: u8) -> WorkspacePin {
        WorkspacePin::new(
            store_id,
            workspace_id,
            Uuid::now_v7(),
            PathBuf::from(format!("/workspace/{workspace_id}")),
            StableWorkspaceRoots {
                basis: view(byte),
                observed_label_binding: view(byte),
                selected_frontier: Some(view(byte + 1)),
                selected_change: None,
                materialized: Some(MaterializationRoots {
                    repository_head: RepositoryRoot {
                        generation: 1,
                        envelope: envelope(byte),
                    },
                    view: view(byte + 1),
                    tree: tree(byte),
                }),
            },
        )
    }

    fn registry() -> (tempfile::TempDir, WorkspaceRegistry, Uuid) {
        let root = tempfile::tempdir().unwrap();
        let store_id = Uuid::now_v7();
        let registry = WorkspaceRegistry::new(root.path(), store_id).unwrap();
        (root, registry, store_id)
    }

    #[test]
    fn round_trip_and_atomic_replacement() {
        let (_root, registry, store_id) = registry();
        let workspace_id = Uuid::now_v7();
        let first = pin(store_id, workspace_id, 1);
        registry.publish(&first).unwrap();
        assert_eq!(registry.read(workspace_id).unwrap(), first);

        let mut replacement = pin(store_id, workspace_id, 7);
        replacement.registration_token = first.registration_token;
        replacement.workspace_path.clone_from(&first.workspace_path);
        registry.publish(&replacement).unwrap();
        assert_eq!(registry.read(workspace_id).unwrap(), replacement);
        assert_eq!(registry.enumerate().unwrap(), vec![replacement]);
    }

    #[test]
    fn rejects_oversize_corrupt_and_mismatched_documents() {
        let (root, registry, store_id) = registry();
        let workspace_id = Uuid::now_v7();
        registry.publish(&pin(store_id, workspace_id, 1)).unwrap();
        let path = root
            .path()
            .join(REGISTRY_DIRECTORY)
            .join(document_name(workspace_id));

        fs::write(&path, vec![b'x'; MAX_DOCUMENT_BYTES as usize + 1]).unwrap();
        assert!(
            registry
                .read(workspace_id)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        fs::write(&path, b"{ definitely not json }").unwrap();
        assert!(registry.read(workspace_id).is_err());

        let other_id = Uuid::now_v7();
        let bytes = serde_json::to_vec(&pin(store_id, other_id, 2)).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(
            registry
                .read(workspace_id)
                .unwrap_err()
                .to_string()
                .contains("filename identity")
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_lock_document_and_registry_directory() {
        use std::os::unix::fs::symlink;

        let (root, registry, store_id) = registry();
        let target = root.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, root.path().join(REGISTRY_LOCK)).unwrap();
        assert!(registry.publish(&pin(store_id, Uuid::now_v7(), 1)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");

        fs::remove_file(root.path().join(REGISTRY_LOCK)).unwrap();
        let workspace_id = Uuid::now_v7();
        registry.publish(&pin(store_id, workspace_id, 2)).unwrap();
        let path = root
            .path()
            .join(REGISTRY_DIRECTORY)
            .join(document_name(workspace_id));
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(registry.read(workspace_id).is_err());

        fs::remove_dir_all(root.path().join(REGISTRY_DIRECTORY)).unwrap();
        symlink(root.path(), root.path().join(REGISTRY_DIRECTORY)).unwrap();
        assert!(registry.enumerate().is_err());
    }

    #[test]
    fn enumeration_is_deterministic_and_strictly_bounded() {
        let (root, registry, store_id) = registry();
        let mut ids = vec![Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        for (index, id) in ids.iter().rev().enumerate() {
            registry
                .publish(&pin(store_id, *id, index as u8 + 1))
                .unwrap();
        }
        ids.sort_unstable();
        assert_eq!(
            registry
                .enumerate()
                .unwrap()
                .iter()
                .map(|pin| pin.workspace_id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(registry.root_set().unwrap().repository_envelopes.len(), 3);

        fs::write(
            root.path().join(REGISTRY_DIRECTORY).join("unexpected"),
            b"x",
        )
        .unwrap();
        assert!(registry.enumerate().is_err());
    }

    #[test]
    fn enumeration_rejects_more_than_the_entry_bound() {
        let (root, registry, _store_id) = registry();
        let directory = root.path().join(REGISTRY_DIRECTORY);
        fs::create_dir(&directory).unwrap();
        for index in 0..=MAX_REGISTRY_ENTRIES {
            File::create(directory.join(format!(".workspace-pin-{index}.tmp"))).unwrap();
        }
        assert!(
            registry
                .enumerate()
                .unwrap_err()
                .to_string()
                .contains("entry limit")
        );
    }

    #[test]
    fn root_union_includes_stable_and_both_pending_sides() {
        let store_id = Uuid::now_v7();
        let mut pin = pin(store_id, Uuid::now_v7(), 1);
        pin.pending_materialization = Some(MaterializationRoots {
            repository_head: RepositoryRoot {
                generation: 2,
                envelope: envelope(3),
            },
            view: view(4),
            tree: tree(5),
        });
        pin.pending_repository_cas = Some(PendingRepositoryCasRoots {
            expected_head: RepositoryRoot {
                generation: 2,
                envelope: envelope(3),
            },
            proposed_head: RepositoryRoot {
                generation: 3,
                envelope: envelope(6),
            },
            resulting_frontier: view(7),
        });
        let roots = pin.roots();
        assert_eq!(
            roots.repository_envelopes,
            BTreeSet::from([envelope(1), envelope(3), envelope(6)])
        );
        assert_eq!(
            roots.views,
            BTreeSet::from([view(1), view(2), view(4), view(7)])
        );
        assert_eq!(roots.trees, BTreeSet::from([tree(1), tree(5)]));
    }
}
