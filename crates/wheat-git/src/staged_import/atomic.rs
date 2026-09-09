use std::{
    ffi::{CString, OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_store::{
    Durability, LocalPublicationStore, ObjectStoreError, PackedObjectStore, RepositoryHead,
    sync_directory, sync_filesystem,
};

use super::{
    FinalizeImportError, StagedImportError, finalize::finalize_import_deferred,
    stage_import_deferred,
};

const MARKER_NAME: &str = "import.marker";
const MARKER: &[u8] = b"wheat-atomic-import-v1\n";
const STAGE_SUFFIX: &[u8] = b".tmp";

#[derive(Debug, Error)]
pub enum AtomicImportError {
    #[error("atomic import destination has no parent directory")]
    MissingParent,
    #[error("atomic import destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("atomic import destination is already locked: {0}")]
    AlreadyLocked(PathBuf),
    #[error("atomic import filesystem operation failed")]
    Io(#[from] io::Error),
    #[error("atomic import staging failed")]
    Stage(#[from] StagedImportError),
    #[error("atomic import finalization failed")]
    Finalize(#[from] FinalizeImportError),
    #[error("atomic import storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("installed atomic import failed validation: {0}")]
    Validation(String),
}

/// Imports a Git repository into a fresh directory with one terminal durability checkpoint.
///
/// In-progress work is isolated in a hidden sibling directory and is intentionally not
/// resumable after a crash. The destination becomes visible only after the complete repository
/// has been validated and durably flushed. Existing destinations are never replaced.
pub async fn atomic_import(
    git_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    working_branch: &str,
) -> Result<RepositoryHead, AtomicImportError> {
    let git_path = git_path.as_ref();
    let destination = destination.as_ref();
    let parent = destination
        .parent()
        .ok_or(AtomicImportError::MissingParent)?;
    let name = destination
        .file_name()
        .ok_or(AtomicImportError::MissingParent)?;
    if !parent.is_dir() {
        return Err(
            io::Error::new(io::ErrorKind::NotFound, "destination parent is missing").into(),
        );
    }

    let stage_prefix = private_stage_prefix(name);
    let lock_path = parent.join(private_lock_name(name));
    let lock = DestinationLock::acquire(lock_path)?;
    if destination.exists() {
        return Err(AtomicImportError::DestinationExists(
            destination.to_path_buf(),
        ));
    }

    remove_stale_stages(parent, &stage_prefix)?;

    let mut staging_name = stage_prefix.clone();
    staging_name.push(Uuid::new_v4().to_string());
    staging_name.push(OsStr::from_bytes(STAGE_SUFFIX));
    let staging = parent.join(staging_name);
    fs::create_dir(&staging)?;
    let mut cleanup = StageCleanup::new(staging.clone());
    write_stage_marker(&staging)?;

    let head = {
        let objects = PackedObjectStore::open_deferred(staging.join("storage"))?;
        let content = objects.content_store();
        let journal = staging.join("import.sqlite");
        let publications = LocalPublicationStore::with_durability(
            staging.join("repository.head"),
            Durability::Deferred,
        );

        loop {
            let progress =
                stage_import_deferred(git_path, &journal, &objects, &content, None).await?;
            if progress.complete {
                break;
            }
        }
        let head = finalize_import_deferred(
            git_path,
            &journal,
            &objects,
            &content,
            &publications,
            working_branch,
        )
        .await?;
        // Finalization has just validated this exact envelope against the isolated
        // backing store. Close every content/SQLite/pack handle before the terminal
        // barrier so buffered writes and close errors cannot cross the atomic rename.
        drop(content);
        drop(publications);
        objects.close()?;
        sync_filesystem(&staging)?;
        head
    };

    rename_noreplace(&staging, destination)?;
    cleanup.disarm();
    sync_directory(parent)?;
    drop(lock);
    Ok(head)
}

fn private_stage_prefix(destination_name: &OsStr) -> OsString {
    let mut prefix = OsString::from(".");
    prefix.push(destination_name);
    prefix.push(".wheat-import-");
    prefix
}

fn private_lock_name(destination_name: &OsStr) -> OsString {
    let mut name = OsString::from(".");
    name.push(destination_name);
    name.push(".wheat-import.lock");
    name
}

fn write_stage_marker(staging: &Path) -> io::Result<()> {
    let marker_path = staging.join(MARKER_NAME);
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker_path)?;
    marker.write_all(MARKER)?;
    marker.sync_all()?;
    sync_directory(staging)
}

fn remove_stale_stages(parent: &Path, prefix: &OsStr) -> io::Result<()> {
    let prefix = prefix.as_bytes();
    let mut removed_any = false;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.as_bytes();
        if !name.starts_with(prefix) || !name.ends_with(STAGE_SUFFIX) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() || !has_exact_stage_marker(&entry.path()) {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
        removed_any = true;
    }
    if removed_any {
        sync_directory(parent)?;
    }
    Ok(())
}

fn has_exact_stage_marker(candidate: &Path) -> bool {
    let marker = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(candidate.join(MARKER_NAME))
    {
        Ok(marker) => marker,
        Err(_) => return false,
    };
    let metadata = match marker.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.file_type().is_file() || metadata.len() != MARKER.len() as u64 {
        return false;
    }
    let mut contents = [0_u8; MARKER.len()];
    let mut marker = marker;
    marker.read_exact(&mut contents).is_ok() && contents == MARKER
}

struct DestinationLock {
    _file: File,
}

impl DestinationLock {
    fn acquire(path: PathBuf) -> Result<Self, AtomicImportError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(Self { _file: file })
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                Err(AtomicImportError::AlreadyLocked(path))
            } else {
                Err(error.into())
            }
        }
    }
}

struct StageCleanup {
    path: Option<PathBuf>,
}

impl StageCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StageCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
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
