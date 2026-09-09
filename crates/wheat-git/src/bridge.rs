//! Orchestration for preparing and publishing a Wheat Git projection.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

use thiserror::Error;
use wheat_core::ViewId;
use wheat_store::{ContentStore, ImmutableObjectStore};

use crate::{
    export::{ExportError, ExportOptions, ExportResult, export_revision_history},
    push::{
        PushError, PushMode, PushReport, RemoteState, cache_observed_ref, is_ancestor,
        observe_remote_ref, push_ref,
    },
};

pub const PROJECTION_REF: &str = "refs/wheat/push";

#[derive(Debug)]
pub struct PreparedPush {
    pub projection_path: PathBuf,
    pub source_ref: &'static str,
    pub destination_ref: String,
    pub remote_state: RemoteState,
    pub export: ExportResult,
    remote_url: String,
    pub force_with_lease: bool,
    _lock: ProjectionLock,
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("Git projection failed")]
    Export(#[from] ExportError),
    #[error("Git transport preparation failed")]
    Push(#[from] PushError),
    #[error("cannot initialize Git projection at {path}: {message}")]
    Initialize { path: PathBuf, message: String },
    #[error("cannot lock Git projection at {path}: {message}")]
    Lock { path: PathBuf, message: String },
    #[error("remote {destination} points at unknown commit {oid}; refusing to rewrite it")]
    UnknownRemoteTip { destination: String, oid: String },
    #[error("selected Wheat view has no revision applications to project")]
    EmptyProjection,
    #[error("prepared Git push was rejected: {0}")]
    Rejected(String),
}

/// Prepare a deterministic Git projection without changing the remote.
pub async fn prepare_push<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    selected: ViewId,
    base: ViewId,
    projection_path: &Path,
    remote_url: &str,
    destination: &str,
    force_with_lease: bool,
) -> Result<PreparedPush, BridgeError> {
    let lock = ProjectionLock::acquire(projection_path)?;
    let remote_state = observe_remote_ref(remote_url, destination)?;
    let git = open_or_initialize_bare(projection_path)?;
    cache_observed_ref(projection_path, remote_url, destination, &remote_state)?;

    let mut options = ExportOptions {
        projected_anchors: read_projected_anchors(&git)?,
        ..ExportOptions::default()
    };
    if let RemoteState::Present(oid) = &remote_state {
        options.anchor = Some(oid.object_id()?);
    }

    let export = match export_revision_history(objects, content, selected, base, &git, &options)
        .await
    {
        Ok(export) => export,
        Err(ExportError::UnmatchedAnchor(_)) | Err(ExportError::MissingAnchor(_)) => {
            options.anchor = None;
            let complete =
                export_revision_history(objects, content, selected, base, &git, &options).await?;
            if !force_with_lease {
                let tip = complete.tip.ok_or(BridgeError::EmptyProjection)?;
                let RemoteState::Present(oid) = &remote_state else {
                    unreachable!("an unmatched anchor requires a present remote ref")
                };
                if !is_ancestor(projection_path, oid, tip)? {
                    return Err(BridgeError::UnknownRemoteTip {
                        destination: destination.into(),
                        oid: oid.as_str().into(),
                    });
                }
            }
            complete
        }
        Err(error) => return Err(error.into()),
    };
    let tip = export.tip.ok_or(BridgeError::EmptyProjection)?;
    git.reference(
        PROJECTION_REF,
        tip,
        gix::refs::transaction::PreviousValue::Any,
        "prepare Wheat Git projection",
    )
    .map_err(|error| BridgeError::Initialize {
        path: projection_path.into(),
        message: error.to_string(),
    })?;
    for (application, oid) in &export.applications {
        git.reference(
            format!("refs/wheat/applications/{application}"),
            *oid,
            gix::refs::transaction::PreviousValue::Any,
            "record Wheat application projection",
        )
        .map_err(|error| BridgeError::Initialize {
            path: projection_path.into(),
            message: error.to_string(),
        })?;
    }

    Ok(PreparedPush {
        projection_path: projection_path.into(),
        source_ref: PROJECTION_REF,
        destination_ref: destination.into(),
        remote_state,
        export,
        remote_url: remote_url.into(),
        force_with_lease,
        _lock: lock,
    })
}

/// Publish an already-prepared projection to its explicitly selected destination.
pub fn publish_prepared(prepared: PreparedPush) -> Result<PushReport, BridgeError> {
    let mode = match (&prepared.remote_state, prepared.force_with_lease) {
        (RemoteState::Present(oid), true) => PushMode::ForceWithLease(oid.clone()),
        (RemoteState::Absent, true) => PushMode::ForceWithLeaseAbsent,
        _ => PushMode::Normal,
    };
    let report = push_ref(
        &prepared.projection_path,
        &prepared.remote_url,
        prepared.source_ref,
        &prepared.destination_ref,
        mode,
    )?;
    if !report.accepted() {
        let detail = report.stderr.to_string_lossy();
        return Err(BridgeError::Rejected(if detail.is_empty() {
            "git did not accept the destination update".into()
        } else {
            detail
        }));
    }
    Ok(report)
}

fn read_projected_anchors(
    git: &gix::Repository,
) -> Result<std::collections::BTreeMap<wheat_core::ApplicationId, gix::ObjectId>, BridgeError> {
    let mut mappings = std::collections::BTreeMap::new();
    let platform = git.references().map_err(|error| BridgeError::Initialize {
        path: git.path().into(),
        message: error.to_string(),
    })?;
    let references = platform
        .prefixed("refs/wheat/applications/")
        .map_err(|error| BridgeError::Initialize {
            path: git.path().into(),
            message: error.to_string(),
        })?;
    for reference in references {
        let reference = reference.map_err(|error| BridgeError::Initialize {
            path: git.path().into(),
            message: error.to_string(),
        })?;
        let name = reference.name().as_bstr().to_string();
        let Some(application) = name.strip_prefix("refs/wheat/applications/") else {
            continue;
        };
        let Ok(application) = application.parse() else {
            continue;
        };
        let target = reference.target();
        let Some(oid) = target.try_id() else {
            continue;
        };
        let oid = oid.to_owned();
        if git.find_commit(oid).is_ok() {
            mappings.insert(application, oid);
        }
    }
    Ok(mappings)
}

#[derive(Debug)]
struct ProjectionLock {
    file: File,
}

impl ProjectionLock {
    fn acquire(projection: &Path) -> Result<Self, BridgeError> {
        let lock_path = projection.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| BridgeError::Lock {
                path: lock_path.clone(),
                message: error.to_string(),
            })?;
        }
        let file = File::create(&lock_path).map_err(|error| BridgeError::Lock {
            path: lock_path.clone(),
            message: error.to_string(),
        })?;
        let result = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            let message = if error.kind() == std::io::ErrorKind::WouldBlock {
                "another Wheat Git projection is already in progress".into()
            } else {
                error.to_string()
            };
            return Err(BridgeError::Lock {
                path: lock_path,
                message,
            });
        }
        Ok(Self { file })
    }
}

impl Drop for ProjectionLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN) };
    }
}

fn open_or_initialize_bare(path: &Path) -> Result<gix::Repository, BridgeError> {
    if path.exists() {
        return gix::open(path).map_err(|error| BridgeError::Initialize {
            path: path.into(),
            message: error.to_string(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| BridgeError::Initialize {
            path: path.into(),
            message: error.to_string(),
        })?;
    }
    gix::init_bare(path).map_err(|error| BridgeError::Initialize {
        path: path.into(),
        message: error.to_string(),
    })
}
