//! A repository on disk, as a server serves it: a packed store in `storage/` and
//! the publication head in `repository.head`.
//!
//! A repository is created whole or not at all. It is built in a hidden sibling
//! (`.{name}.wheat-stage-{id}.tmp`) and renamed into place once complete, and it is
//! deleted by renaming it to a hidden tombstone (`.{name}.wheat-deleted-{id}.tmp`)
//! first, so it vanishes at once. Both hold `.{name}.wheat.lock` meanwhile, and both
//! sweep the stages and tombstones a crash left under that name. A swept directory
//! must hold the marker file this module writes, so nothing else is ever removed.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_core::{CanonicalObjectError, Repository, Tree, ViewLabelManifestRecord};

use crate::{
    LocalPublicationStore, ObjectStoreError, PackedObjectStore, RepositoryHead,
    RepositoryPublicationError, RepositoryPublisher, sync_directory, sync_filesystem,
};

const MARKER_NAME: &str = "stage.marker";
const MARKER: &[u8] = b"wheat-repository-stage-v1\n";
const STAGE_INFIX: &str = ".wheat-stage-";
const TOMBSTONE_INFIX: &str = ".wheat-deleted-";
const SWEPT_INFIX: &str = ".wheat-";
const HIDDEN_SUFFIX: &str = ".tmp";

#[derive(Debug, Error)]
pub enum RepositoryDirectoryError {
    #[error("repository directory has no parent directory")]
    MissingParent,
    #[error("repository directory already exists: {0}")]
    Exists(PathBuf),
    #[error("repository directory does not exist: {0}")]
    Missing(PathBuf),
    #[error("not a repository directory: {0}")]
    NotARepository(PathBuf),
    #[error("another create or delete holds repository directory {0}")]
    Locked(PathBuf),
    #[error("repository is open: {0}")]
    InUse(PathBuf),
    #[error("invalid first label")]
    Label(#[source] CanonicalObjectError),
    #[error("repository directory I/O failed")]
    Io(#[from] io::Error),
    #[error("repository storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("publishing the first envelope failed")]
    Publication(#[from] RepositoryPublicationError),
}

/// A repository directory: `storage/` and `repository.head` under one root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryDirectory {
    root: PathBuf,
}

impl RepositoryDirectory {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The packed store's directory.
    #[must_use]
    pub fn storage(&self) -> PathBuf {
        self.root.join("storage")
    }

    /// The publication head's file.
    #[must_use]
    pub fn head(&self) -> PathBuf {
        self.root.join("repository.head")
    }

    /// The publication head.
    #[must_use]
    pub fn publications(&self) -> LocalPublicationStore {
        LocalPublicationStore::new(self.head())
    }

    /// Opens the packed store. It stays locked against other opens and against
    /// [`Self::delete`] until closed or dropped.
    pub fn open_store(&self) -> Result<PackedObjectStore, ObjectStoreError> {
        PackedObjectStore::open(self.storage())
    }

    /// Creates the repository with a first envelope over an empty tree, in which
    /// `label` names the base view. Fails if the directory exists.
    pub async fn create_empty(
        &self,
        label: &str,
    ) -> Result<RepositoryHead, RepositoryDirectoryError> {
        let repository = Repository::new(Tree::new());
        let labels = ViewLabelManifestRecord::new([(label, repository.base_view().id)])
            .map_err(RepositoryDirectoryError::Label)?;
        let mut pending = self.begin_create()?;
        let stage = pending.directory().clone();
        let head = {
            let objects = stage.open_store()?;
            let content = objects.content_store();
            let publications = stage.publications();
            let publisher = RepositoryPublisher::new(&objects, &content, &publications);
            let closure = publisher.prepare(&repository).await?;
            let envelope = publisher
                .prepare_envelope(closure.state.id, &labels)
                .await?;
            let head = publisher.initialize_prepared_envelope(envelope).await?;
            drop(content);
            objects.close()?;
            head
        };
        sync_filesystem(stage.root())?;
        pending.install()?;
        pending.sync_parent()?;
        Ok(head)
    }

    /// Starts creating this repository: takes its name's lock, refuses an existing
    /// directory, sweeps what crashes left, and makes an empty hidden stage to build
    /// it in. [`PendingRepository::install`] puts it in place; dropping the
    /// [`PendingRepository`] first removes the stage.
    pub fn begin_create(&self) -> Result<PendingRepository, RepositoryDirectoryError> {
        let (parent, name) = self.parent_and_name()?;
        if !parent.is_dir() {
            return Err(
                io::Error::new(io::ErrorKind::NotFound, "repository parent is missing").into(),
            );
        }
        let lock = NameLock::acquire(parent.join(hidden_name(name, ".wheat.lock")))?;
        if self.root.exists() {
            return Err(RepositoryDirectoryError::Exists(self.root.clone()));
        }
        sweep(parent, name)?;
        let stage = parent.join(hidden_unique(name, STAGE_INFIX));
        fs::create_dir(&stage)?;
        let pending = PendingRepository {
            _lock: lock,
            stage: Self::new(stage),
            destination: self.root.clone(),
            parent: parent.to_path_buf(),
            installed: false,
        };
        write_marker(pending.stage.root())?;
        Ok(pending)
    }

    /// Deletes the repository. Refuses while its store is open, here or in another
    /// process, and anything that is not a repository directory.
    pub fn delete(&self) -> Result<(), RepositoryDirectoryError> {
        let (parent, name) = self.parent_and_name()?;
        let _lock = NameLock::acquire(parent.join(hidden_name(name, ".wheat.lock")))?;
        sweep(parent, name)?;
        if !self.root.exists() {
            return Err(RepositoryDirectoryError::Missing(self.root.clone()));
        }
        let catalog_lock = self.storage().join("packed").join("catalog.lock");
        if !self.head().is_file() || !catalog_lock.is_file() {
            return Err(RepositoryDirectoryError::NotARepository(self.root.clone()));
        }
        // The packed store holds this lock while open. Holding it through the rename
        // keeps the store from being opened before it is gone.
        let store_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&catalog_lock)?;
        match store_lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(RepositoryDirectoryError::InUse(self.root.clone()));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        write_marker(&self.root)?;
        let tombstone = parent.join(hidden_unique(name, TOMBSTONE_INFIX));
        fs::rename(&self.root, &tombstone)?;
        sync_directory(parent)?;
        drop(store_lock);
        fs::remove_dir_all(&tombstone)?;
        sync_directory(parent)?;
        Ok(())
    }

    fn parent_and_name(&self) -> Result<(&Path, &OsStr), RepositoryDirectoryError> {
        let parent = self
            .root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(RepositoryDirectoryError::MissingParent)?;
        let name = self
            .root
            .file_name()
            .ok_or(RepositoryDirectoryError::MissingParent)?;
        Ok((parent, name))
    }
}

/// A repository being built in a hidden stage; see
/// [`RepositoryDirectory::begin_create`].
#[derive(Debug)]
pub struct PendingRepository {
    _lock: NameLock,
    stage: RepositoryDirectory,
    destination: PathBuf,
    parent: PathBuf,
    installed: bool,
}

impl PendingRepository {
    /// The stage, to build the repository in.
    #[must_use]
    pub fn directory(&self) -> &RepositoryDirectory {
        &self.stage
    }

    /// Renames the stage into place, never over an existing directory. The caller
    /// first closes the store and makes the stage durable (`sync_filesystem`).
    pub fn install(&mut self) -> Result<(), RepositoryDirectoryError> {
        match rename_noreplace(self.stage.root(), &self.destination) {
            Ok(()) => {
                self.installed = true;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(RepositoryDirectoryError::Exists(self.destination.clone()))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Makes the installing rename durable.
    pub fn sync_parent(&self) -> io::Result<()> {
        sync_directory(&self.parent)
    }
}

impl Drop for PendingRepository {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_dir_all(self.stage.root());
        }
    }
}

/// `.{name}{suffix}`.
fn hidden_name(name: &OsStr, suffix: &str) -> OsString {
    let mut hidden = OsString::from(".");
    hidden.push(name);
    hidden.push(suffix);
    hidden
}

/// `.{name}{infix}{a new id}.tmp`.
fn hidden_unique(name: &OsStr, infix: &str) -> OsString {
    let mut hidden = hidden_name(name, infix);
    hidden.push(Uuid::new_v4().to_string());
    hidden.push(HIDDEN_SUFFIX);
    hidden
}

fn write_marker(directory: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut marker = options.open(directory.join(MARKER_NAME))?;
    marker.write_all(MARKER)?;
    marker.sync_all()?;
    sync_directory(directory)
}

/// Removes the stages and tombstones of `name` a crash left in `parent`.
fn sweep(parent: &Path, name: &OsStr) -> io::Result<()> {
    let prefix = hidden_name(name, SWEPT_INFIX);
    let prefix = prefix.as_encoded_bytes();
    let mut removed = false;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let candidate = file_name.as_encoded_bytes();
        if !candidate.starts_with(prefix) || !candidate.ends_with(HIDDEN_SUFFIX.as_bytes()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() || !has_exact_marker(&entry.path()) {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
        removed = true;
    }
    if removed {
        sync_directory(parent)?;
    }
    Ok(())
}

/// Off Unix a marker cannot be opened without following a symlink, so no
/// directory counts as marked and none is swept.
#[cfg(not(unix))]
fn has_exact_marker(_candidate: &Path) -> bool {
    false
}

#[cfg(unix)]
fn has_exact_marker(candidate: &Path) -> bool {
    use std::io::Read as _;

    let Ok(mut marker) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(candidate.join(MARKER_NAME))
    else {
        return false;
    };
    let Ok(metadata) = marker.metadata() else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.len() != MARKER.len() as u64 {
        return false;
    }
    let mut contents = [0_u8; MARKER.len()];
    marker.read_exact(&mut contents).is_ok() && contents == MARKER
}

/// The lock one create or delete of a name holds.
#[derive(Debug)]
struct NameLock {
    _file: File,
}

impl NameLock {
    fn acquire(path: PathBuf) -> Result<Self, RepositoryDirectoryError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(RepositoryDirectoryError::Locked(path)),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "renaming without replacing needs Linux's renameat2",
    ))
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
