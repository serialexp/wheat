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
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_core::{CanonicalObjectError, Repository, Tree, ViewLabelManifestRecord};

use crate::packed::files::StoreFile;
use crate::{
    LocalPublicationStore, ObjectStoreError, PackedObjectStore, RepositoryHead,
    RepositoryPublicationError, RepositoryPublisher, sync_directory, sync_filesystem,
};

pub(crate) const MARKER_NAME: &str = "stage.marker";
const MARKER: &[u8] = b"wheat-repository-stage-v1\n";
const STAGE_INFIX: &str = ".wheat-stage-";
pub(crate) const TOMBSTONE_INFIX: &str = ".wheat-deleted-";
const SWEPT_INFIX: &str = ".wheat-";
pub(crate) const HIDDEN_SUFFIX: &str = ".tmp";

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

    /// Whether a repository is here: its head and its packed store's.
    #[must_use]
    pub fn is_repository(&self) -> bool {
        self.head().is_file() && self.storage().join("packed").join("catalog.head").is_file()
    }

    /// The files that make up the repository, for copying it: its head and its
    /// store's state, without locks, temporary files, or the store's locator index
    /// (rebuilt when a copy is opened). The list and the files agree only while
    /// nothing writes to the repository; a server holds it quiesced meanwhile
    /// (`wheat_server::registry::Registry::quiesce`). Reading may go on. A store
    /// open with deferred durability (`PackedObjectStore::open_deferred`, never a
    /// server's) keeps what it writes ahead in a segment it publishes only when
    /// closed; a copy of it holds what a crash would leave.
    pub fn files(&self) -> io::Result<Vec<RepositoryFile>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            if file_type.is_dir() && name == "storage" {
                list_store_files(&entry.path(), Path::new(&name), &mut files)?;
            } else if file_type.is_file() && !is_repository_leftover(&name) {
                files.push(RepositoryFile::new(&entry, PathBuf::from(name), false)?);
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Copies the repository to `destination` (see [`Self::files`] for when that is
    /// consistent), which must not exist yet. The copy is made in a hidden stage
    /// and renamed into place once complete and durable, like [`Self::create_empty`].
    /// Returns the files copied.
    pub fn copy_to(
        &self,
        destination: &RepositoryDirectory,
    ) -> Result<Vec<RepositoryFile>, RepositoryDirectoryError> {
        let files = self.files()?;
        copy_files(&self.root, &files, destination)?;
        Ok(files)
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
        let mut pending = self.begin_create()?;
        let stage = pending.directory().clone();
        let head = initialize_empty(&stage.storage(), &stage.publications(), label).await?;
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
        let lock = NameLock::of(parent, name)?;
        if self.root.exists() {
            return Err(RepositoryDirectoryError::Exists(self.root.clone()));
        }
        PendingRepository::stage(lock, parent, name, self.root.clone())
    }

    /// Deletes the repository. Refuses while its store is open, here or in another
    /// process, and anything that is not a repository directory.
    pub fn delete(&self) -> Result<(), RepositoryDirectoryError> {
        let (parent, name) = self.parent_and_name()?;
        let _lock = NameLock::of(parent, name)?;
        sweep(parent, name)?;
        if !self.root.exists() {
            return Err(RepositoryDirectoryError::Missing(self.root.clone()));
        }
        if !self.is_repository() {
            return Err(RepositoryDirectoryError::NotARepository(self.root.clone()));
        }
        // Held through the rename, it keeps the store from being opened before it is
        // gone.
        let store_lock = lock_closed_store(&self.storage(), &self.root)?;
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

/// Publishes, through `publications`, a first envelope over an empty tree in a new
/// packed store in `storage`, in which `label` names the base view.
pub(crate) async fn initialize_empty(
    storage: &Path,
    publications: &LocalPublicationStore,
    label: &str,
) -> Result<RepositoryHead, RepositoryDirectoryError> {
    let repository = Repository::new(Tree::new());
    let labels = ViewLabelManifestRecord::new([(label, repository.base_view().id)])
        .map_err(RepositoryDirectoryError::Label)?;
    let objects = PackedObjectStore::open(storage)?;
    let content = objects.content_store();
    let publisher = RepositoryPublisher::new(&objects, &content, publications);
    let closure = publisher.prepare(&repository).await?;
    let envelope = publisher
        .prepare_envelope(closure.state.id, &labels)
        .await?;
    let head = publisher.initialize_prepared_envelope(envelope).await?;
    drop(content);
    objects.close()?;
    Ok(head)
}

/// Copies `files`, listed under `source`, to `destination`, which must not exist yet:
/// into a hidden stage, renamed into place once complete and durable.
pub(crate) fn copy_files(
    source: &Path,
    files: &[RepositoryFile],
    destination: &RepositoryDirectory,
) -> Result<(), RepositoryDirectoryError> {
    let mut pending = destination.begin_create()?;
    let stage = pending.directory().root().to_path_buf();
    for file in files {
        let target = stage.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut reader = File::open(source.join(&file.path))?.take(file.len);
        let copied = io::copy(&mut reader, &mut File::create_new(&target)?)?;
        if copied != file.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} shrank while it was copied", file.path.display()),
            )
            .into());
        }
    }
    sync_filesystem(&stage)?;
    pending.install()?;
    pending.sync_parent()?;
    Ok(())
}

/// Takes the lock the packed store in `storage` holds while open, refusing with
/// [`RepositoryDirectoryError::InUse`] (naming `owner`) while it is. A copy has no
/// lock file until its store is first opened.
pub(crate) fn lock_closed_store(
    storage: &Path,
    owner: &Path,
) -> Result<File, RepositoryDirectoryError> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(storage.join("packed").join("catalog.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => {
            Err(RepositoryDirectoryError::InUse(owner.to_path_buf()))
        }
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// One file of a repository, as [`RepositoryDirectory::files`] lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFile {
    /// Its path under the repository's root.
    pub path: PathBuf,
    /// Its length when listed. A copy takes this much of it.
    pub len: u64,
    /// Whether it never changes once written, so a copy made earlier that has it has
    /// all of it: an incremental backup uploads it once.
    pub immutable: bool,
}

impl RepositoryFile {
    pub(crate) fn new(entry: &fs::DirEntry, path: PathBuf, immutable: bool) -> io::Result<Self> {
        Ok(Self {
            path,
            len: entry.metadata()?.len(),
            immutable,
        })
    }
}

/// Whether `name`, in a repository's root, is something other than its state: the
/// head's lock, a stage's marker, or a head being written.
pub(crate) fn is_repository_leftover(name: &OsStr) -> bool {
    name == "repository.lock"
        || name == MARKER_NAME
        || name.as_encoded_bytes().ends_with(HIDDEN_SUFFIX.as_bytes())
}

/// Adds the files of the packed store under `directory`, whose path under the
/// repository is `relative`, to `files`.
pub(crate) fn list_store_files(
    directory: &Path,
    relative: &Path,
    files: &mut Vec<RepositoryFile>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = relative.join(entry.file_name());
        if file_type.is_dir() {
            list_store_files(&entry.path(), &path, files)?;
        } else if file_type.is_file() {
            let in_store = path.strip_prefix("storage").unwrap_or(&path);
            match crate::packed::files::classify(in_store) {
                StoreFile::Skipped => {}
                StoreFile::Immutable => files.push(RepositoryFile::new(&entry, path, true)?),
                StoreFile::Mutable => files.push(RepositoryFile::new(&entry, path, false)?),
            }
        }
    }
    Ok(())
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
    /// Sweeps what crashes left under `name` in `parent`, whose lock `lock` is, and
    /// makes an empty hidden stage there to be installed at `destination`.
    pub(crate) fn stage(
        lock: NameLock,
        parent: &Path,
        name: &OsStr,
        destination: PathBuf,
    ) -> Result<Self, RepositoryDirectoryError> {
        sweep(parent, name)?;
        let stage = parent.join(hidden_unique(name, STAGE_INFIX));
        fs::create_dir(&stage)?;
        let pending = Self {
            _lock: lock,
            stage: RepositoryDirectory::new(stage),
            destination,
            parent: parent.to_path_buf(),
            installed: false,
        };
        write_marker(pending.stage.root())?;
        Ok(pending)
    }

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
pub(crate) fn hidden_unique(name: &OsStr, infix: &str) -> OsString {
    let mut hidden = hidden_name(name, infix);
    hidden.push(Uuid::new_v4().to_string());
    hidden.push(HIDDEN_SUFFIX);
    hidden
}

pub(crate) fn write_marker(directory: &Path) -> io::Result<()> {
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
pub(crate) fn sweep(parent: &Path, name: &OsStr) -> io::Result<()> {
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
pub(crate) struct NameLock {
    _file: File,
}

impl NameLock {
    /// The lock of `name` in `parent`: `.{name}.wheat.lock`.
    pub(crate) fn of(parent: &Path, name: &OsStr) -> Result<Self, RepositoryDirectoryError> {
        Self::acquire(parent.join(hidden_name(name, ".wheat.lock")))
    }

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
