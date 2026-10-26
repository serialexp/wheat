use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use thiserror::Error;
use wheat_store::{
    Durability, LocalPublicationStore, ObjectStoreError, PackedObjectStore,
    PackedStoreTraceSnapshot, RepositoryDirectory, RepositoryDirectoryError, RepositoryHead,
    sync_filesystem,
};

use super::{
    FinalizeImportError, FinalizePhase, StagedImportError,
    finalize::finalize_import_deferred_with_observer, stage_import_deferred,
};

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

impl From<RepositoryDirectoryError> for AtomicImportError {
    fn from(error: RepositoryDirectoryError) -> Self {
        match error {
            RepositoryDirectoryError::MissingParent => Self::MissingParent,
            RepositoryDirectoryError::Exists(path) => Self::DestinationExists(path),
            RepositoryDirectoryError::Locked(path) => Self::AlreadyLocked(path),
            RepositoryDirectoryError::Io(error) => Self::Io(error),
            RepositoryDirectoryError::Store(error) => Self::Store(error),
            // Creating and installing a stage reports none of the rest.
            error => Self::Io(io::Error::other(error)),
        }
    }
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

    observer(AtomicImportEvent::PhaseStarted {
        phase: AtomicImportPhase::LockAndCleanup,
    });
    let phase_started = Instant::now();
    let mut pending = RepositoryDirectory::new(destination.as_ref()).begin_create()?;
    let stage = pending.directory().clone();
    let staging = stage.root();
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
            let (objects, trace) = PackedObjectStore::open_deferred_traced(stage.storage())?;
            (objects, Some(trace))
        } else {
            (PackedObjectStore::open_deferred(stage.storage())?, None)
        };
        observer(AtomicImportEvent::PhaseFinished {
            phase: AtomicImportPhase::OpenStore,
            elapsed: phase_started.elapsed(),
        });
        let content = objects.content_store();
        let journal = staging.join("import.sqlite");
        let publications =
            LocalPublicationStore::with_durability(stage.head(), Durability::Deferred);

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
        sync_filesystem(staging)?;
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
    pending.install()?;
    observer(AtomicImportEvent::PhaseFinished {
        phase: AtomicImportPhase::Install,
        elapsed: phase_started.elapsed(),
    });
    observer(AtomicImportEvent::PhaseStarted {
        phase: AtomicImportPhase::ParentSync,
    });
    let phase_started = Instant::now();
    pending.sync_parent()?;
    observer(AtomicImportEvent::PhaseFinished {
        phase: AtomicImportPhase::ParentSync,
        elapsed: phase_started.elapsed(),
    });
    drop(pending);
    Ok(head)
}
