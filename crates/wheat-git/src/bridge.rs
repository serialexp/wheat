//! Orchestration for preparing and publishing a Wheat Git projection.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

use thiserror::Error;
use wheat_core::{ChangeMapId, LabelAnnotation, ViewId};
use wheat_store::{ContentStore, ImmutableObjectStore};

use crate::{
    export::{
        ExportAnchor, ExportError, ExportOptions, ExportResult, export_revision_history, write_tag,
    },
    push::{
        PushError, PushMode, PushReport, RemoteState, cache_observed_ref, fetch_remote_heads,
        is_ancestor, observe_remote_ref, observe_remote_tag, push_ref, push_tag,
        remote_heads_contain,
    },
};

pub const PROJECTION_REF: &str = "refs/wheat/push";
/// Local refs naming prepared tag objects, one per tag name.
pub const TAG_SOURCE_PREFIX: &str = "refs/wheat/tags/";

/// An annotated tag ready to create on a remote, or already there.
#[derive(Debug)]
pub struct PreparedTagPush {
    pub projection_path: PathBuf,
    pub source_ref: String,
    /// Full remote ref, `refs/tags/<name>`.
    pub tag_ref: String,
    pub tag_object: gix::ObjectId,
    pub commit: gix::ObjectId,
    /// The remote already has exactly this tag object.
    pub up_to_date: bool,
    remote_url: String,
    _lock: ProjectionLock,
}

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
    #[error(
        "tag {tag} would point at commit {commit}, which is on no branch of the remote; push the branch first"
    )]
    TagCommitNotOnRemoteBranch { tag: String, commit: String },
    #[error("remote already has {tag} as tag object {remote}, not {local}; tags are never moved")]
    TagExists {
        tag: String,
        remote: String,
        local: String,
    },
}

/// Prepare a deterministic Git projection without changing the remote.
///
/// `changes` is the pinned repository state's logical-change map, from which each
/// commit's message is built.
pub async fn prepare_push<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    selected: ViewId,
    base: ViewId,
    changes: ChangeMapId,
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
        options.anchor = ExportAnchor::Commit(oid.object_id()?);
    }

    let export =
        match export_revision_history(objects, content, selected, base, changes, &git, &options)
            .await
        {
            Ok(export) => export,
            Err(ExportError::UnmatchedAnchor(_)) | Err(ExportError::MissingAnchor(_)) => {
                options.anchor = ExportAnchor::None;
                let complete = export_revision_history(
                    objects, content, selected, base, changes, &git, &options,
                )
                .await?;
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

/// Prepare annotated tag `name` for `selected` without changing the remote.
///
/// The commit comes from one fixed rule, [`ExportAnchor::NewestImported`], so it
/// depends only on the view, and the tag object only on that commit, the name and the
/// stored `annotation`: the same label gives the same object IDs on any machine, any
/// time. The tag is refused unless its commit is already reachable from a branch on
/// the remote, which catches any drift between this rule and how the branch was
/// pushed. An existing remote tag is accepted only if it is this exact tag object.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_tag_push<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    selected: ViewId,
    base: ViewId,
    changes: ChangeMapId,
    annotation: &LabelAnnotation,
    projection_path: &Path,
    remote_url: &str,
    name: &str,
) -> Result<PreparedTagPush, BridgeError> {
    let tag_ref = format!("refs/tags/{name}");
    let lock = ProjectionLock::acquire(projection_path)?;
    let remote_state = observe_remote_tag(remote_url, &tag_ref)?;
    let git = open_or_initialize_bare(projection_path)?;
    fetch_remote_heads(projection_path, remote_url)?;

    let options = ExportOptions {
        anchor: ExportAnchor::NewestImported,
        ..ExportOptions::default()
    };
    let not_on_branch = |commit: gix::ObjectId| BridgeError::TagCommitNotOnRemoteBranch {
        tag: tag_ref.clone(),
        commit: commit.to_string(),
    };
    let export =
        match export_revision_history(objects, content, selected, base, changes, &git, &options)
            .await
        {
            Ok(export) => export,
            // The imported commit is on no fetched branch, or it would be here.
            Err(ExportError::MissingAnchor(commit)) => return Err(not_on_branch(commit)),
            Err(error) => return Err(error.into()),
        };
    let commit = export.tip.ok_or(BridgeError::EmptyProjection)?;
    if !remote_heads_contain(projection_path, commit)? {
        return Err(not_on_branch(commit));
    }
    let tag_object = write_tag(&git, commit, name, annotation)?;
    let up_to_date = match &remote_state {
        RemoteState::Absent => false,
        RemoteState::Present(remote) if remote.object_id()? == tag_object => true,
        RemoteState::Present(remote) => {
            return Err(BridgeError::TagExists {
                tag: tag_ref,
                remote: remote.as_str().into(),
                local: tag_object.to_string(),
            });
        }
    };
    let source_ref = format!("{TAG_SOURCE_PREFIX}{name}");
    git.reference(
        source_ref.as_str(),
        tag_object,
        gix::refs::transaction::PreviousValue::Any,
        "prepare Wheat Git tag",
    )
    .map_err(|error| BridgeError::Initialize {
        path: projection_path.into(),
        message: error.to_string(),
    })?;

    Ok(PreparedTagPush {
        projection_path: projection_path.into(),
        source_ref,
        tag_ref,
        tag_object,
        commit,
        up_to_date,
        remote_url: remote_url.into(),
        _lock: lock,
    })
}

/// Create a prepared tag on the remote. Returns `None` when it was already there.
pub fn publish_tag(prepared: PreparedTagPush) -> Result<Option<PushReport>, BridgeError> {
    if prepared.up_to_date {
        return Ok(None);
    }
    let report = push_tag(
        &prepared.projection_path,
        &prepared.remote_url,
        &prepared.source_ref,
        &prepared.tag_ref,
    )?;
    if !report.accepted() {
        let detail = report.stderr.to_string_lossy();
        return Err(BridgeError::Rejected(if detail.is_empty() {
            "git did not accept the tag".into()
        } else {
            detail
        }));
    }
    Ok(Some(report))
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
        let file_name = projection.file_name().ok_or_else(|| BridgeError::Lock {
            path: projection.into(),
            message: "projection path has no file name".into(),
        })?;
        let mut lock_name = file_name.to_os_string();
        lock_name.push(".lock");
        let lock_path = projection.with_file_name(lock_name);
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

#[cfg(test)]
mod tests {
    use super::ProjectionLock;

    #[test]
    fn lock_names_preserve_the_complete_projection_name() {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("projection");
        let dotted = directory.path().join("projection.git");
        let _plain_lock = ProjectionLock::acquire(&plain).unwrap();
        let _dotted_lock = ProjectionLock::acquire(&dotted).unwrap();
        assert!(directory.path().join("projection.lock").is_file());
        assert!(directory.path().join("projection.git.lock").is_file());
    }

    #[test]
    fn lock_is_exclusive_until_the_holder_drops() {
        let directory = tempfile::tempdir().unwrap();
        let projection = directory.path().join("projection.git");
        let first = ProjectionLock::acquire(&projection).unwrap();
        assert!(ProjectionLock::acquire(&projection).is_err());
        drop(first);
        ProjectionLock::acquire(&projection).unwrap();
    }
}
