#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_store::{
    Durability, LocalPublicationStore, ObjectStoreError, PackedObjectStore,
    PackedStoreTraceSnapshot, RepositoryHead, sync_directory, sync_filesystem,
};

use super::{
    FinalizeImportError, FinalizePhase, StagedImportError,
    finalize::finalize_import_deferred_with_observer, stage_import_deferred,
};

const MARKER_NAME: &str = "import.marker";
const MARKER: &[u8] = b"wheat-atomic-import-v1\n";
const STAGE_SUFFIX: &str = ".tmp";

/// Stable coarse boundaries and bounded progress emitted by a traced atomic import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicImportEvent {
    PhaseStarted {
        phase: AtomicImportPhase,
    },
    PhaseFinished {
        phase: AtomicImportPhase,
        elapsed: Duration,
    },
    StageProgress {
        batch: u64,
        elapsed: Duration,
        complete: bool,
        completed_commits: usize,
        pending_commits: usize,
        discovery_pending: bool,
        branches: usize,
        storage: PackedStoreTraceSnapshot,
    },
    FinalizePhaseStarted {
        phase: FinalizePhase,
    },
    FinalizePhaseFinished {
        phase: FinalizePhase,
        elapsed: Duration,
        storage: PackedStoreTraceSnapshot,
    },
    ValidationPhaseStarted {
        phase: wheat_store::RepositoryValidationPhase,
    },
    ValidationPhaseFinished {
        phase: wheat_store::RepositoryValidationPhase,
        elapsed: Duration,
        storage: PackedStoreTraceSnapshot,
    },
    StorageClosed {
        storage: PackedStoreTraceSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicImportPhase {
    LockAndCleanup,
    OpenStore,
    Stage,
    Finalize,
    CloseStore,
    FilesystemSync,
    Install,
    ParentSync,
}

impl AtomicImportPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LockAndCleanup => "lock_and_cleanup",
            Self::OpenStore => "open_store",
            Self::Stage => "stage",
            Self::Finalize => "finalize",
            Self::CloseStore => "close_store",
            Self::FilesystemSync => "filesystem_sync",
            Self::Install => "install",
            Self::ParentSync => "parent_sync",
        }
    }
}

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
    atomic_import_inner(git_path, destination, working_branch, |_| {}, false).await
}

/// Imports while synchronously reporting bounded phase, progress, and storage diagnostics.
pub async fn atomic_import_with_observer<F>(
    git_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    working_branch: &str,
    observer: F,
) -> Result<RepositoryHead, AtomicImportError>
where
    F: FnMut(AtomicImportEvent),
{
    atomic_import_inner(git_path, destination, working_branch, observer, true).await
}

async fn atomic_import_inner<F>(
    git_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    working_branch: &str,
    mut observer: F,
    traced: bool,
) -> Result<RepositoryHead, AtomicImportError>
where
    F: FnMut(AtomicImportEvent),
{
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

    observer(AtomicImportEvent::PhaseStarted {
        phase: AtomicImportPhase::LockAndCleanup,
    });
    let phase_started = Instant::now();
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
    staging_name.push(STAGE_SUFFIX);
    let staging = parent.join(staging_name);
    fs::create_dir(&staging)?;
    let mut cleanup = StageCleanup::new(staging.clone());
    write_stage_marker(&staging)?;
    observer(AtomicImportEvent::PhaseFinished {
        phase: AtomicImportPhase::LockAndCleanup,
        elapsed: phase_started.elapsed(),
    });

    let head = {
        observer(AtomicImportEvent::PhaseStarted {
            phase: AtomicImportPhase::OpenStore,
        });
        let phase_started = Instant::now();
        let (objects, trace) = if traced {
            let (objects, trace) =
                PackedObjectStore::open_deferred_traced(staging.join("storage"))?;
            (objects, Some(trace))
        } else {
            (
                PackedObjectStore::open_deferred(staging.join("storage"))?,
                None,
            )
        };
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::OpenStore,
            elapsed: phase_started.elapsed(),
        });
        let content = objects.content_store();
        let journal = staging.join("import.sqlite");
        let publications = LocalPublicationStore::with_durability(
            staging.join("repository.head"),
            Durability::Deferred,
        );

        observer(AtomicImportEvent::PhaseStarted {
            phase: AtomicImportPhase::Stage,
        });
        let stage_started = Instant::now();
        let mut batch = 0_u64;
        loop {
            let batch_started = Instant::now();
            let progress =
                stage_import_deferred(git_path, &journal, &objects, &content, None).await?;
            if let Some(trace) = &trace {
                observer(AtomicImportEvent::StageProgress {
                    batch,
                    elapsed: batch_started.elapsed(),
                    complete: progress.complete,
                    completed_commits: progress.completed_commits,
                    pending_commits: progress.pending_commits,
                    discovery_pending: progress.discovery_pending,
                    branches: progress.branches.len(),
                    storage: trace.snapshot().expect("store remains open"),
                });
            }
            batch = batch.saturating_add(1);
            if progress.complete {
                break;
            }
        }
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::Stage,
            elapsed: stage_started.elapsed(),
        });
        observer(AtomicImportEvent::PhaseStarted {
            phase: AtomicImportPhase::Finalize,
        });
        let phase_started = Instant::now();
        let previous_finalize = std::cell::Cell::new(None::<(FinalizePhase, Instant)>);
        let previous_validation =
            std::cell::Cell::new(None::<(wheat_store::RepositoryValidationPhase, Instant)>);
        let head = {
            let shared_observer = std::cell::RefCell::new(&mut observer);
            let head = finalize_import_deferred_with_observer(
                git_path,
                &journal,
                &objects,
                &content,
                &publications,
                working_branch,
                |phase| {
                    if let (Some((previous, started)), Some(trace)) =
                        (previous_finalize.get(), trace.as_ref())
                    {
                        shared_observer.borrow_mut()(AtomicImportEvent::FinalizePhaseFinished {
                            phase: previous,
                            elapsed: started.elapsed(),
                            storage: trace.snapshot().expect("store remains open"),
                        });
                    }
                    shared_observer.borrow_mut()(AtomicImportEvent::FinalizePhaseStarted { phase });
                    previous_finalize.set(Some((phase, Instant::now())));
                },
                |phase| {
                    if let (Some((previous, started)), Some(trace)) =
                        (previous_validation.get(), trace.as_ref())
                    {
                        shared_observer.borrow_mut()(AtomicImportEvent::ValidationPhaseFinished {
                            phase: previous,
                            elapsed: started.elapsed(),
                            storage: trace.snapshot().expect("store remains open"),
                        });
                    }
                    shared_observer.borrow_mut()(AtomicImportEvent::ValidationPhaseStarted {
                        phase,
                    });
                    previous_validation.set(Some((phase, Instant::now())));
                },
            )
            .await?;
            if let (Some((phase, started)), Some(trace)) =
                (previous_validation.get(), trace.as_ref())
            {
                shared_observer.borrow_mut()(AtomicImportEvent::ValidationPhaseFinished {
                    phase,
                    elapsed: started.elapsed(),
                    storage: trace.snapshot().expect("store remains open"),
                });
            }
            if let (Some((phase, started)), Some(trace)) = (previous_finalize.get(), trace.as_ref())
            {
                shared_observer.borrow_mut()(AtomicImportEvent::FinalizePhaseFinished {
                    phase,
                    elapsed: started.elapsed(),
                    storage: trace.snapshot().expect("store remains open"),
                });
            }
            head
        };
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::Finalize,
            elapsed: phase_started.elapsed(),
        });
        // Finalization has just validated this exact envelope against the isolated
        // backing store. Close every content/SQLite/pack handle before the terminal
        // barrier so buffered writes and close errors cannot cross the atomic rename.
        drop(content);
        drop(publications);
        observer(AtomicImportEvent::PhaseStarted {
            phase: AtomicImportPhase::CloseStore,
        });
        let phase_started = Instant::now();
        let storage = objects.close_with_trace()?;
        if let Some(storage) = storage {
            observer(AtomicImportEvent::StorageClosed { storage });
        }
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::CloseStore,
            elapsed: phase_started.elapsed(),
        });
        observer(AtomicImportEvent::PhaseStarted {
            phase: AtomicImportPhase::FilesystemSync,
        });
        let phase_started = Instant::now();
        sync_filesystem(&staging)?;
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::FilesystemSync,
            elapsed: phase_started.elapsed(),
        });
        head
    };

    observer(AtomicImportEvent::PhaseStarted {
        phase: AtomicImportPhase::Install,
    });
    let phase_started = Instant::now();
    rename_noreplace(&staging, destination)?;
    cleanup.disarm();
    observer(AtomicImportEvent::PhaseFinished {
        phase: AtomicImportPhase::Install,
        elapsed: phase_started.elapsed(),
    });
    observer(AtomicImportEvent::PhaseStarted {
        phase: AtomicImportPhase::ParentSync,
    });
    let phase_started = Instant::now();
    sync_directory(parent)?;
    observer(AtomicImportEvent::PhaseFinished {
        phase: AtomicImportPhase::ParentSync,
        elapsed: phase_started.elapsed(),
    });
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut marker = options.open(marker_path)?;
    marker.write_all(MARKER)?;
    marker.sync_all()?;
    sync_directory(staging)
}

fn remove_stale_stages(parent: &Path, prefix: &OsStr) -> io::Result<()> {
    let prefix = prefix.as_encoded_bytes();
    let mut removed_any = false;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if !name.starts_with(prefix) || !name.ends_with(STAGE_SUFFIX.as_bytes()) {
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

/// Off Unix a marker cannot be opened without following a symlink, so no
/// directory counts as a stage and none is removed.
#[cfg(not(unix))]
fn has_exact_stage_marker(_candidate: &Path) -> bool {
    false
}

#[cfg(unix)]
fn has_exact_stage_marker(candidate: &Path) -> bool {
    use std::io::Read as _;

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
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(AtomicImportError::AlreadyLocked(path)),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
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
