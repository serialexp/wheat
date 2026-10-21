//! Pulling a Git branch into a Wheat repository.
//!
//! [`prepare_pull`] fetches the branch into the store's Git projection and walks back
//! from its tip along first parents to the newest commit the workspace's view already
//! holds. A commit is known either as one the repository imported (its change is the
//! one the importer derives from the commit ID, and its revision records the commit as
//! provenance) or as one a push projected (the repository state's projected commits
//! name the application, whichever store pushed it). Either way it must be on the
//! view's own path: the application found must be the one the view holds for that
//! change. Git IDs only recognise commits; which changes the repository holds is
//! decided by its own records.
//!
//! The commits above the known one become change proposals, oldest first, exactly as
//! the importer would write them: one change per first-parent commit, with its edits
//! against the first parent. A merge is thus one change, and its side branch's commits
//! are not changes of their own. The caller appends them with
//! [`wheat_store::PersistedRepositoryMutator::append_and_rebase`] while it holds the
//! returned [`PreparedPull`], whose lock keeps a push or another pull out of the
//! projection.

use std::path::Path;

use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, ChangeProposal, ProjectedCommitSetId, ViewId,
    edit_application::ApplicationLimits,
};
use wheat_store::{
    ApplicationMapError, ApplicationMapStore, CollectionError, ContentStore, ImmutableObjectStore,
    ProjectedCommitSetStore, RepositoryPublicationError, RepositoryReader, RevisionMapError,
    RevisionMapStore,
};

use crate::{
    StagedImportError,
    bridge::{BridgeError, ProjectionLock, open_or_initialize_bare},
    export::commit_key,
    push::{PushError, RemoteState, cache_observed_ref, observe_remote_ref},
    staged_import::{commit_proposal, import_application_limits, import_change_id},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PullLimits {
    /// New commits one pull imports. Walking past this many without reaching a known
    /// commit is refused.
    pub max_commits: usize,
}

impl Default for PullLimits {
    fn default() -> Self {
        Self {
            max_commits: 10_000,
        }
    }
}

/// How the newest known commit was recognised.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnownBy {
    /// The repository imported it.
    Import,
    /// A push projected `ApplicationId` as it.
    Push(ApplicationId),
}

/// A pull ready to append. Holding it keeps the projection locked.
#[derive(Debug)]
pub struct PreparedPull {
    pub remote_tip: gix::ObjectId,
    /// The newest commit the view holds, and its change.
    pub onto_commit: gix::ObjectId,
    pub onto: ChangeId,
    pub known_by: KnownBy,
    /// The commits above `onto_commit` as proposals, oldest first. Empty when the view
    /// is up to date.
    pub commits: Vec<ChangeProposal>,
    /// Limits for applying their edits: the importer's, which allow larger files than
    /// local authoring's.
    pub application_limits: ApplicationLimits,
    _lock: ProjectionLock,
}

#[derive(Debug, Error)]
pub enum PullError {
    #[error("Git projection failed")]
    Bridge(#[from] BridgeError),
    #[error("fetching from the Git remote failed")]
    Transport(#[from] PushError),
    #[error("reading the pulled Git commits failed")]
    Import(#[from] StagedImportError),
    #[error("reading the Wheat repository failed")]
    Repository(#[from] RepositoryPublicationError),
    #[error("reading the Wheat repository failed")]
    ProjectedCommits(#[from] CollectionError),
    #[error("reading the Wheat repository failed")]
    RevisionMap(#[from] RevisionMapError),
    #[error("reading the Wheat repository failed")]
    ApplicationMap(#[from] ApplicationMapError),
    #[error("cannot read Git commit {commit} in the projection: {message}")]
    Git { commit: String, message: String },
    #[error("remote {remote} has no {branch} to pull")]
    RemoteBranchAbsent { remote: String, branch: String },
    #[error(
        "{branch} on {remote} shares no commit with this workspace's history; it may be \
         unrelated, or its common commit reachable only through a merge's second parent"
    )]
    NoCommonCommit { remote: String, branch: String },
    #[error(
        "{branch} on {remote} has more than {limit} commits this workspace lacks; pull \
         fewer at a time or import the repository afresh"
    )]
    TooManyCommits {
        remote: String,
        branch: String,
        limit: usize,
    },
    #[error(
        "Git commit {commit} is Wheat change {change}, but this workspace holds that change \
         elsewhere in its history; pull into a workspace on the history the commit came from"
    )]
    KnownCommitOffPath { commit: String, change: ChangeId },
}

/// Fetches `branch` from `remote_url` and finds the commits `effective` lacks; see the
/// module documentation. `projected_commits` is the pinned repository state's, and
/// `projection_path` the store's Git projection.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prepare_pull<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    effective: ViewId,
    projected_commits: ProjectedCommitSetId,
    projection_path: &Path,
    remote_url: &str,
    branch: &str,
    limits: PullLimits,
) -> Result<PreparedPull, PullError> {
    let lock = ProjectionLock::acquire(projection_path)?;
    let remote_state = tracing::debug_span!("git_observe_remote")
        .in_scope(|| observe_remote_ref(remote_url, branch))?;
    let RemoteState::Present(tip) = &remote_state else {
        return Err(PullError::RemoteBranchAbsent {
            remote: remote_url.into(),
            branch: branch.into(),
        });
    };
    let remote_tip = tip.object_id()?;
    // Initialize the projection first: the fetch writes into it.
    open_or_initialize_bare(projection_path)?;
    tracing::debug_span!("git_fetch")
        .in_scope(|| cache_observed_ref(projection_path, remote_url, branch, &remote_state))?;
    // Open it again after the fetch so its new packs are visible.
    let git = open_or_initialize_bare(projection_path)?;

    let reader = RepositoryReader::new(objects, content);
    let view = reader.view(effective).await?;
    let known = Known {
        objects,
        content,
        projected_commits,
        view: &view,
    };
    let mut new_commits = Vec::new();
    let mut commit = remote_tip;
    let (onto, known_by) = loop {
        if let Some(found) = known.recognise(commit).await? {
            break found;
        }
        if new_commits.len() == limits.max_commits {
            return Err(PullError::TooManyCommits {
                remote: remote_url.into(),
                branch: branch.into(),
                limit: limits.max_commits,
            });
        }
        new_commits.push(commit);
        let object = find_commit(&git, commit)?;
        let Some(parent) = object.parent_ids().next() else {
            return Err(PullError::NoCommonCommit {
                remote: remote_url.into(),
                branch: branch.into(),
            });
        };
        commit = parent.detach();
    };
    let onto_commit = commit;

    let mut previous = onto;
    let mut commits = Vec::with_capacity(new_commits.len());
    for commit in new_commits.into_iter().rev() {
        let proposal = tracing::debug_span!("pull_commit_proposal")
            .in_scope(|| commit_proposal(&git, commit, previous, content))?;
        previous = proposal.id;
        commits.push(proposal);
    }
    Ok(PreparedPull {
        remote_tip,
        onto_commit,
        onto,
        known_by,
        commits,
        application_limits: import_application_limits(),
        _lock: lock,
    })
}

fn find_commit(git: &gix::Repository, commit: gix::ObjectId) -> Result<gix::Commit<'_>, PullError> {
    git.find_commit(commit).map_err(|error| PullError::Git {
        commit: commit.to_string(),
        message: error.to_string(),
    })
}

/// Recognises the commits a view already holds.
struct Known<'a, O> {
    objects: &'a O,
    content: &'a ContentStore,
    projected_commits: ProjectedCommitSetId,
    view: &'a wheat_core::ViewRecord,
}

impl<O: ImmutableObjectStore> Known<'_, O> {
    /// The change the view holds `commit` as, or `None` when the view doesn't hold it.
    #[tracing::instrument(name = "pull_recognise", level = "debug", skip_all)]
    async fn recognise(
        &self,
        commit: gix::ObjectId,
    ) -> Result<Option<(ChangeId, KnownBy)>, PullError> {
        if let Some(change) = self.imported(commit).await? {
            return Ok(Some((change, KnownBy::Import)));
        }
        self.pushed(commit).await
    }

    /// The change `commit` was imported as, when the view holds it at its import.
    ///
    /// The importer authors each commit's revision against its first parent's view and
    /// applies it right there, so a view whose application of the change targets the
    /// revision's authored view, with its authored tree, holds exactly the commit's
    /// first-parent history below it.
    async fn imported(&self, commit: gix::ObjectId) -> Result<Option<ChangeId>, PullError> {
        let change = import_change_id(commit.as_bytes());
        let Some(revision_id) = RevisionMapStore::new(self.objects)
            .get(self.view.revisions, change)
            .await?
        else {
            return Ok(None);
        };
        let reader = RepositoryReader::new(self.objects, self.content);
        let revision = reader.revision(revision_id).await?;
        let off_path = || PullError::KnownCommitOffPath {
            commit: commit.to_string(),
            change,
        };
        let provenance = revision
            .metadata
            .provenance
            .as_ref()
            .filter(|p| p.system == "git-sha1" && p.object_id == commit.as_bytes());
        if provenance.is_none() {
            return Err(off_path());
        }
        let application = ApplicationMapStore::new(self.objects)
            .get(self.view.applications, change)
            .await?
            .ok_or_else(off_path)?;
        let application = reader.application(application).await?;
        let at_import = application.target_view == revision.authored_against
            && application.result
                == (ApplicationResultRecord::Clean {
                    tree: revision.resulting_tree,
                });
        if !at_import {
            return Err(off_path());
        }
        Ok(Some(change))
    }

    /// The change a push projected as `commit`, when the view holds that application.
    async fn pushed(
        &self,
        commit: gix::ObjectId,
    ) -> Result<Option<(ChangeId, KnownBy)>, PullError> {
        let Some(key) = commit_key(commit) else {
            return Ok(None);
        };
        let candidates = ProjectedCommitSetStore::new(self.objects)
            .applications(self.projected_commits, key)
            .await?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let reader = RepositoryReader::new(self.objects, self.content);
        let applications = ApplicationMapStore::new(self.objects);
        let mut elsewhere = None;
        for candidate in candidates {
            let application = reader.application(candidate).await?;
            let change = reader.revision(application.revision_id).await?.change_id;
            if applications.get(self.view.applications, change).await? != Some(candidate)
                || !matches!(application.result, ApplicationResultRecord::Clean { .. })
            {
                elsewhere.get_or_insert(change);
                continue;
            }
            return Ok(Some((change, KnownBy::Push(candidate))));
        }
        match elsewhere {
            Some(change) => Err(PullError::KnownCommitOffPath {
                commit: commit.to_string(),
                change,
            }),
            None => Ok(None),
        }
    }
}
