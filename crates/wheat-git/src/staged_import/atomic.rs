use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
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

const MARKER: &str = "wheat-atomic-import-v1\n";

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

    let lock_path = parent.join(format!(".{}.wheat-import.lock", name.to_string_lossy()));
    let lock = DestinationLock::acquire(lock_path)?;
    if destination.exists() {
        return Err(AtomicImportError::DestinationExists(
            destination.to_path_buf(),
        ));
    }

    let staging = parent.join(format!(
        ".{}.wheat-import-{}.tmp",
        name.to_string_lossy(),
        Uuid::new_v4()
    ));
    fs::create_dir(&staging)?;
    let mut cleanup = StageCleanup::new(staging.clone());
    fs::write(staging.join("import.marker"), MARKER)?;

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
        // backing store. The terminal barrier makes those validated bytes durable.
        sync_filesystem(&staging)?;
        head
    };

    rename_noreplace(&staging, destination)?;
    cleanup.disarm();
    sync_directory(parent)?;
    drop(lock);
    Ok(head)
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
