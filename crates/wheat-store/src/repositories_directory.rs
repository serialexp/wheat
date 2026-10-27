//! A server's repositories on disk, grouped in fork networks.
//!
//! ```text
//! <root>/
//!   names/<owner>/<repo>                    where the repository is
//!   networks/<network>/
//!     storage/                              the network's packed store
//!     heads/<repository>/repository.head    each repository's head
//! ```
//!
//! A network is a repository and every fork made from it or from its forks. Its
//! repositories share its store, so a fork copies no objects: it is a head naming
//! its parent's envelope. Networks and repositories are named by UUIDs; a name
//! (`names/<owner>/<repo>`, a `RepositoryLocationV1` record) says which network and
//! which head are the repository's.
//!
//! Creating, forking and deleting hold the name's lock
//! (`names/<owner>/.<repo>.wheat.lock`). A new network is built in a hidden stage
//! beside its name and renamed into `networks/` once complete, and a deleted network
//! or head is renamed to a hidden tombstone beside its name first, so the sweep a
//! [`RepositoryDirectory`] does under its name's lock cleans up after crashes here
//! too. The name is written last when creating and removed last when deleting, so a
//! crash leaves at worst a network or head no name refers to (for network GC, not
//! yet written) or a name whose head is gone, which counts as no repository and is
//! replaced by the next create or fork of that name.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    RepositoryLocationRecord, StoreRecordError, decode_repository_location,
    encode_repository_location,
};

use crate::repository_directory::{
    HIDDEN_SUFFIX, NameLock, PendingRepository, TOMBSTONE_INFIX, copy_files, hidden_unique,
    initialize_empty, is_repository_leftover, list_store_files, lock_closed_store, sweep,
    write_marker,
};
use crate::{
    LocalPublicationStore, ObjectStoreError, PackedObjectStore, PublicationError, PublicationStore,
    RepositoryDirectory, RepositoryDirectoryError, RepositoryFile, RepositoryHead, sync_directory,
    sync_filesystem,
};

const NAMES: &str = "names";
const NETWORKS: &str = "networks";
const HEADS: &str = "heads";
const HEAD_FILE: &str = "repository.head";
/// A name record being written: `.{repo}.wheat-name-{id}.tmp`.
const NAME_INFIX: &str = ".wheat-name-";

#[derive(Debug, Error)]
pub enum RepositoriesDirectoryError {
    #[error("not a repository name: {0:?}")]
    InvalidName(String),
    #[error("there is no repository {0}")]
    Missing(String),
    #[error("repository {0} already exists")]
    Exists(String),
    #[error("repository location {} is invalid", .0.display())]
    Location(PathBuf, #[source] StoreRecordError),
    #[error("reading the parent repository's head failed")]
    Head(#[from] PublicationError),
    #[error(transparent)]
    Directory(#[from] RepositoryDirectoryError),
    #[error("repositories directory I/O failed")]
    Io(#[from] io::Error),
}

/// A repository [`RepositoriesDirectory`] created: where it is, and its head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacedRepository {
    pub location: RepositoryLocationRecord,
    pub head: RepositoryHead,
}

/// A server's repositories; see the module documentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoriesDirectory {
    root: PathBuf,
}

impl RepositoriesDirectory {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The network `network`'s directory.
    #[must_use]
    pub fn network(&self, network: Uuid) -> NetworkDirectory {
        NetworkDirectory::new(
            self.root
                .join(NETWORKS)
                .join(network.as_simple().to_string()),
        )
    }

    /// Where the repository `name` (`owner/repo`) is, or `None` when there is none.
    pub fn locate(
        &self,
        name: &str,
    ) -> Result<Option<RepositoryLocationRecord>, RepositoriesDirectoryError> {
        let (parent, repo) = self.name_path(name)?;
        self.live_location(&parent, repo)
    }

    /// Creates the repository `name` in a network of its own, with a first envelope
    /// over an empty tree in which `label` names the base view.
    pub async fn create_empty(
        &self,
        name: &str,
        label: &str,
    ) -> Result<PlacedRepository, RepositoriesDirectoryError> {
        let (parent, repo) = self.name_path(name)?;
        let networks = self.root.join(NETWORKS);
        fs::create_dir_all(&parent)?;
        fs::create_dir_all(&networks)?;
        let lock = NameLock::of(&parent, repo)?;
        sweep_name_records(&parent, repo)?;
        if self.live_location(&parent, repo)?.is_some() {
            return Err(RepositoriesDirectoryError::Exists(name.to_owned()));
        }
        let location = RepositoryLocationRecord {
            network: Uuid::new_v4(),
            repository: Uuid::new_v4(),
        };
        let mut pending = PendingRepository::stage(
            lock,
            &parent,
            repo,
            self.network(location.network).root().to_path_buf(),
        )?;
        let stage = NetworkDirectory::new(pending.directory().root());
        fs::create_dir_all(stage.head_directory(location.repository))?;
        let head = initialize_empty(
            &stage.storage(),
            &stage.publications(location.repository),
            label,
        )
        .await?;
        sync_filesystem(stage.root())?;
        pending.install()?;
        sync_directory(&networks)?;
        write_location(&parent, repo, location)?;
        Ok(PlacedRepository { location, head })
    }

    /// Creates the repository `name` as a fork of `parent`: a new head in `parent`'s
    /// network naming `parent`'s envelope, so it starts with every label `parent`
    /// has. Writes nothing to the network's store.
    pub fn fork(
        &self,
        parent: &str,
        name: &str,
    ) -> Result<PlacedRepository, RepositoriesDirectoryError> {
        let (source_parent, source_repo) = self.name_path(parent)?;
        let (target_parent, target_repo) = self.name_path(name)?;
        if parent == name {
            return Err(RepositoriesDirectoryError::Exists(name.to_owned()));
        }
        // Held so `parent` is not deleted, taking its network with it, meanwhile.
        let _source_lock = NameLock::of(&source_parent, source_repo)
            .map_err(|error| missing_parent(error, parent))?;
        let source = self
            .live_location(&source_parent, source_repo)?
            .ok_or_else(|| RepositoriesDirectoryError::Missing(parent.to_owned()))?;
        fs::create_dir_all(&target_parent)?;
        let _lock = NameLock::of(&target_parent, target_repo)?;
        sweep(&target_parent, target_repo)?;
        sweep_name_records(&target_parent, target_repo)?;
        if self.live_location(&target_parent, target_repo)?.is_some() {
            return Err(RepositoriesDirectoryError::Exists(name.to_owned()));
        }
        let network = self.network(source.network);
        let head =
            RepositoryHead::initial(network.publications(source.repository).read()?.envelope);
        let location = RepositoryLocationRecord {
            network: source.network,
            repository: Uuid::new_v4(),
        };
        fs::create_dir(network.head_directory(location.repository))?;
        network.publications(location.repository).initialize(head)?;
        sync_directory(&network.root.join(HEADS))?;
        write_location(&target_parent, target_repo, location)?;
        Ok(PlacedRepository { location, head })
    }

    /// Deletes the repository `name`: its head, and its network with it when no
    /// other repository is left in it. Refuses while the network's store is open,
    /// here or in another process.
    pub fn delete(&self, name: &str) -> Result<(), RepositoriesDirectoryError> {
        let (parent, repo) = self.name_path(name)?;
        let missing = || RepositoriesDirectoryError::Missing(name.to_owned());
        if !parent.is_dir() {
            return Err(missing());
        }
        let _lock = NameLock::of(&parent, repo)?;
        sweep(&parent, repo)?;
        sweep_name_records(&parent, repo)?;
        let record = parent.join(repo);
        let location = read_location(&record)?.ok_or_else(missing)?;
        let network = self.network(location.network);
        let head = network.head_directory(location.repository);
        if head.is_dir() {
            // Held through the rename, it keeps the network's store from being opened
            // before what goes is gone.
            let store_lock = lock_closed_store(&network.storage(), network.root())?;
            let others = network
                .repositories()?
                .into_iter()
                .any(|repository| repository != location.repository);
            let doomed = if others { head } else { network.root.clone() };
            write_marker(&doomed)?;
            let tombstone = parent.join(hidden_unique(repo, TOMBSTONE_INFIX));
            fs::rename(&doomed, &tombstone)?;
            if let Some(container) = doomed.parent() {
                sync_directory(container)?;
            }
            drop(store_lock);
            fs::remove_file(&record)?;
            sync_directory(&parent)?;
            fs::remove_dir_all(&tombstone)?;
        } else {
            // A crash left the name behind its head.
            fs::remove_file(&record)?;
        }
        sync_directory(&parent)?;
        Ok(())
    }

    /// `names/<owner>` and `<repo>` for `owner/repo`.
    fn name_path<'a>(
        &self,
        name: &'a str,
    ) -> Result<(PathBuf, &'a OsStr), RepositoriesDirectoryError> {
        let mut segments = name.split('/');
        match (segments.next(), segments.next(), segments.next()) {
            (Some(owner), Some(repo), None) if is_name_segment(owner) && is_name_segment(repo) => {
                Ok((self.root.join(NAMES).join(owner), OsStr::new(repo)))
            }
            _ => Err(RepositoriesDirectoryError::InvalidName(name.to_owned())),
        }
    }

    /// The location `<repo>` in `parent` names, when its head is there.
    fn live_location(
        &self,
        parent: &Path,
        repo: &OsStr,
    ) -> Result<Option<RepositoryLocationRecord>, RepositoriesDirectoryError> {
        Ok(read_location(&parent.join(repo))?.filter(|location| {
            self.network(location.network)
                .head(location.repository)
                .is_file()
        }))
    }
}

/// A fork network's directory: its packed store in `storage/`, and each
/// repository's head in `heads/<repository>/repository.head`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkDirectory {
    root: PathBuf,
}

impl NetworkDirectory {
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

    /// The directory of `repository`'s head.
    #[must_use]
    pub fn head_directory(&self, repository: Uuid) -> PathBuf {
        self.root
            .join(HEADS)
            .join(repository.as_simple().to_string())
    }

    /// `repository`'s head file.
    #[must_use]
    pub fn head(&self, repository: Uuid) -> PathBuf {
        self.head_directory(repository).join(HEAD_FILE)
    }

    /// `repository`'s head.
    #[must_use]
    pub fn publications(&self, repository: Uuid) -> LocalPublicationStore {
        LocalPublicationStore::new(self.head(repository))
    }

    /// The repositories with a head here.
    pub fn repositories(&self) -> io::Result<Vec<Uuid>> {
        let entries = match fs::read_dir(self.root.join(HEADS)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut repositories = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(repository) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::try_parse(name).ok())
            {
                repositories.push(repository);
            }
        }
        repositories.sort_unstable();
        Ok(repositories)
    }

    /// Opens the packed store. It stays locked against other opens and against
    /// [`RepositoriesDirectory::delete`] until closed or dropped.
    pub fn open_store(&self) -> Result<PackedObjectStore, ObjectStoreError> {
        PackedObjectStore::open(self.storage())
    }

    /// The files that make up the network, for copying it: its store's state, as
    /// [`RepositoryDirectory::files`] lists it, and every repository's head. The list
    /// and the files agree only while nothing writes to the network; a server holds
    /// it quiesced meanwhile (`wheat_server::registry::Registry::quiesce` on any of
    /// its repositories).
    pub fn files(&self) -> io::Result<Vec<RepositoryFile>> {
        let mut files = Vec::new();
        list_store_files(&self.storage(), Path::new("storage"), &mut files)?;
        for repository in self.repositories()? {
            let directory = Path::new(HEADS).join(repository.as_simple().to_string());
            for entry in fs::read_dir(self.root.join(&directory))? {
                let entry = entry?;
                let name = entry.file_name();
                if entry.file_type()?.is_file() && !is_repository_leftover(&name) {
                    files.push(RepositoryFile::new(&entry, directory.join(name), false)?);
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Copies the network to `destination` (see [`Self::files`] for when that is
    /// consistent), which must not exist yet, like [`RepositoryDirectory::copy_to`].
    /// Returns the files copied.
    pub fn copy_to(
        &self,
        destination: &NetworkDirectory,
    ) -> Result<Vec<RepositoryFile>, RepositoryDirectoryError> {
        let files = self.files()?;
        copy_files(
            &self.root,
            &files,
            &RepositoryDirectory::new(&destination.root),
        )?;
        Ok(files)
    }
}

fn is_name_segment(segment: &str) -> bool {
    !segment.is_empty() && !segment.starts_with('.') && !segment.contains(['\\', '\0'])
}

/// The location the name record at `path` holds, or `None` when there is none.
fn read_location(
    path: &Path,
) -> Result<Option<RepositoryLocationRecord>, RepositoriesDirectoryError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    decode_repository_location(&bytes)
        .map(Some)
        .map_err(|error| RepositoriesDirectoryError::Location(path.to_owned(), error))
}

/// Writes the name record `<repo>` in `parent`, replacing one whose head is gone.
/// The caller holds the name's lock.
fn write_location(
    parent: &Path,
    repo: &OsStr,
    location: RepositoryLocationRecord,
) -> Result<(), RepositoriesDirectoryError> {
    let record = parent.join(repo);
    let bytes = encode_repository_location(location)
        .map_err(|error| RepositoriesDirectoryError::Location(record.clone(), error))?;
    let temporary = parent.join(hidden_unique(repo, NAME_INFIX));
    let written = (|| {
        let mut file = File::create_new(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &record)?;
        sync_directory(parent)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    Ok(written?)
}

/// Removes the name records of `<repo>` a crash left half written in `parent`. The
/// caller holds the name's lock.
fn sweep_name_records(parent: &Path, repo: &OsStr) -> io::Result<()> {
    let mut prefix = OsStr::new(".").to_os_string();
    prefix.push(repo);
    prefix.push(NAME_INFIX);
    let prefix = prefix.as_encoded_bytes();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if name.starts_with(prefix)
            && name.ends_with(HIDDEN_SUFFIX.as_bytes())
            && entry.file_type()?.is_file()
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// A fork's parent that is locked is reported as such; one with no directory for
/// its owner yet does not exist.
fn missing_parent(error: RepositoryDirectoryError, parent: &str) -> RepositoriesDirectoryError {
    match error {
        RepositoryDirectoryError::Io(io) if io.kind() == io::ErrorKind::NotFound => {
            RepositoriesDirectoryError::Missing(parent.to_owned())
        }
        error => error.into(),
    }
}
