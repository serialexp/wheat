//! Resumable import into immutable storage. This module deliberately does not publish a repository head.

use std::{collections::BTreeMap, path::Path};

use gix::bstr::ByteSlice;
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    ApplicationResultRecord, ChangeId, ChangeProposal, SourceProvenance, Transformation,
};
use wheat_store::{
    ApplicationMapStore, CommitWriterError, ContentStore, ContentStoreError, ImmutableObjectStore,
    PersistedCommitWriter, RepositoryReader, RevisionMapStore,
};

use crate::{
    journal::{ImportJournal, JournalError, NativeResult},
    metadata,
};

mod tree_diff;

const NAMESPACE: Uuid = Uuid::from_u128(0xc245_a89b_519f_5a4b_9b36_d62b_78f5_2901);
const SNAPSHOT_VERSION: &[u8] = b"wheat-git-staged-import-v1\0";
const DISCOVERY_BATCH: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedBranch {
    /// Full pinned Git ref name (for example `refs/heads/main`).
    pub name: String,
    pub source_oid: Vec<u8>,
    pub native: Option<NativeResult>,
}

/// Progress in immutable staging only. No repository or branch head was published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedImportResult {
    pub complete: bool,
    pub completed_commits: usize,
    pub pending_commits: usize,
    pub branches: BTreeMap<String, StagedBranch>,
}

#[derive(Debug, Error)]
pub enum StagedImportError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("Git import failed: {0}")]
    Git(String),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error(transparent)]
    Writer(#[from] CommitWriterError),
    #[error("stored native mapping for Git commit {0} is invalid: {1}")]
    InvalidMapping(String, String),
    #[error("Git metadata is not valid UTF-8: {0}")]
    InvalidUtf8(&'static str),
    #[error("Git submodule at {0} is unsupported")]
    UnsupportedSubmodule(String),
}

/// Discover and stage at most `max_commits` additional commits. Discovery itself is durable
/// and bounded-memory. Reopening with changed refs, HEAD, repository path, or format is rejected.
pub async fn stage_import<O: ImmutableObjectStore>(
    git_path: impl AsRef<Path>,
    journal_path: impl AsRef<Path>,
    objects: &O,
    content: &ContentStore,
    max_commits: Option<usize>,
) -> Result<StagedImportResult, StagedImportError> {
    let git = gix::open(git_path.as_ref()).map_err(git_error_local)?;
    let branches = pinned_branches(&git)?;
    let snapshot = snapshot_bytes(&git, git_path.as_ref(), &branches)?;
    let mut journal = ImportJournal::open(journal_path, &snapshot)?;

    for oid in branches.values() {
        add_commit(&git, &mut journal, *oid)?;
    }
    loop {
        let missing = journal.missing_parents(DISCOVERY_BATCH)?;
        if missing.is_empty() {
            break;
        }
        for bytes in missing {
            let oid = gix::ObjectId::from_bytes_or_panic(&bytes);
            add_commit(&git, &mut journal, oid)?;
        }
    }

    let writer = PersistedCommitWriter::new(objects, content);
    let empty = writer.empty_view().await?;
    // A journal is only a hint: authenticate every reused object and relationship first.
    let mut after = None::<Vec<u8>>;
    loop {
        let page = journal.completed(after.as_deref(), DISCOVERY_BATCH)?;
        if page.is_empty() {
            break;
        }
        for (oid, native) in page {
            verify_mapping(objects, content, &journal, empty, &oid, native).await?;
            after = Some(oid);
        }
    }

    let budget = max_commits.unwrap_or(usize::MAX);
    let mut written = 0usize;
    while written < budget {
        let Some(work) = journal.ready(1)?.into_iter().next() else {
            break;
        };
        let oid = gix::ObjectId::from_bytes_or_panic(&work.oid);
        let commit = git.find_commit(oid).map_err(git_error_local)?;
        let parent_native = work
            .parents
            .first()
            .map(|p| journal.result(p))
            .transpose()?
            .flatten();
        let parent_view = parent_native.map_or(empty, |p| p.view_id);
        let edits = tree_diff::tree_edits(&git, &commit, content)?;
        let parents = commit.parent_ids().map(|p| p.detach()).collect::<Vec<_>>();
        let (change_metadata, revision_metadata) =
            metadata(&commit, &work.oid, &parents).map_err(convert_import_error)?;
        let change_id = ChangeId::from_import_identity(NAMESPACE, &work.oid);
        let dependencies = parent_native.map(|p| p.change_id).into_iter().collect();
        let persisted = writer
            .write(
                parent_view,
                ChangeProposal {
                    id: change_id,
                    metadata: change_metadata,
                    dependencies,
                    revision_metadata,
                    transformation: Transformation { edits },
                },
            )
            .await?;
        let native = NativeResult {
            change_id,
            revision_id: persisted.revision_id,
            view_id: persisted.resulting_view,
            application_id: persisted.application_id,
        };
        verify_mapping(objects, content, &journal, empty, &work.oid, native).await?;
        journal.complete(&work.oid, native)?;
        written += 1;
    }
    let (completed_commits, pending_commits) = journal.progress()?;
    let complete = pending_commits == 0;
    let mut output = BTreeMap::new();
    for (name, oid) in branches {
        let source_oid = oid.as_bytes().to_vec();
        output.insert(
            name.clone(),
            StagedBranch {
                name,
                native: journal.result(&source_oid)?,
                source_oid,
            },
        );
    }
    Ok(StagedImportResult {
        complete,
        completed_commits,
        pending_commits,
        branches: output,
    })
}

fn add_commit(
    git: &gix::Repository,
    journal: &mut ImportJournal,
    oid: gix::ObjectId,
) -> Result<(), StagedImportError> {
    let commit = git.find_commit(oid).map_err(git_error_local)?;
    let parents = commit
        .parent_ids()
        .map(|p| p.as_bytes().to_vec())
        .collect::<Vec<_>>();
    journal.add_pending(oid.as_bytes(), &parents)?;
    Ok(())
}

fn pinned_branches(
    git: &gix::Repository,
) -> Result<BTreeMap<String, gix::ObjectId>, StagedImportError> {
    let mut out = BTreeMap::new();
    let platform = git.references().map_err(git_error_local)?;
    let iter = platform.local_branches().map_err(git_error_local)?;
    for reference in iter {
        let mut reference = reference.map_err(git_error_local)?;
        let name = reference
            .name()
            .as_bstr()
            .to_str()
            .map_err(|_| StagedImportError::InvalidUtf8("branch name"))?
            .to_owned();
        out.insert(
            name,
            reference.peel_to_id().map_err(git_error_local)?.detach(),
        );
    }
    Ok(out)
}

fn snapshot_bytes(
    git: &gix::Repository,
    supplied: &Path,
    branches: &BTreeMap<String, gix::ObjectId>,
) -> Result<Vec<u8>, StagedImportError> {
    let canonical =
        std::fs::canonicalize(supplied).map_err(|e| StagedImportError::Git(e.to_string()))?;
    let mut out = SNAPSHOT_VERSION.to_vec();
    push(&mut out, canonical.to_string_lossy().as_bytes());
    let head = std::fs::read(git.path().join("HEAD"))
        .map_err(|e| StagedImportError::Git(e.to_string()))?;
    push(&mut out, &head);
    for (name, oid) in branches {
        push(&mut out, name.as_bytes());
        push(&mut out, oid.as_bytes());
    }
    Ok(out)
}
fn push(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

async fn verify_mapping<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    journal: &ImportJournal,
    empty: wheat_core::ViewId,
    oid: &[u8],
    n: NativeResult,
) -> Result<(), StagedImportError> {
    let bad = |why: &str| StagedImportError::InvalidMapping(hex(oid), why.to_owned());
    if n.change_id != ChangeId::from_import_identity(NAMESPACE, oid) {
        return Err(bad("change identity"));
    }
    let reader = RepositoryReader::new(objects, content);
    let revision = reader
        .revision(n.revision_id)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    let application = reader
        .application(n.application_id)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    let view = reader
        .view(n.view_id)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    let provenance_ok = revision
        .metadata
        .provenance
        .as_ref()
        .is_some_and(|p: &SourceProvenance| p.system == "git-sha1" && p.object_id == oid);
    if revision.change_id != n.change_id
        || !provenance_ok
        || application.revision_id != n.revision_id
    {
        return Err(bad("revision/application relationship or provenance"));
    }
    let parents = journal.parents(oid)?;
    let parent_native = parents
        .first()
        .map(|p| journal.result(p))
        .transpose()?
        .flatten();
    if !parents.is_empty() && parent_native.is_none() {
        return Err(bad("missing completed first parent"));
    }
    if revision.dependencies != parent_native.map(|p| p.change_id).into_iter().collect()
        || revision
            .metadata
            .provenance
            .as_ref()
            .is_none_or(|p| p.parent_object_ids != parents)
    {
        return Err(bad("dependencies or parent provenance"));
    }
    let expected_target = parent_native.map_or(empty, |p| p.view_id);
    if application.target_view != expected_target || revision.authored_against != expected_target {
        return Err(bad("revision/application target view"));
    }
    let tree = match application.result {
        ApplicationResultRecord::Clean { tree } => tree,
        _ => return Err(bad("conflicted application")),
    };
    if tree != revision.resulting_tree || view.tree != tree {
        return Err(bad("resulting tree"));
    }
    if RevisionMapStore::new(objects)
        .get(view.revisions, n.change_id)
        .await
        .map_err(|e| bad(&e.to_string()))?
        != Some(n.revision_id)
        || ApplicationMapStore::new(objects)
            .get(view.applications, n.change_id)
            .await
            .map_err(|e| bad(&e.to_string()))?
            != Some(n.application_id)
    {
        return Err(bad("view maps"));
    }
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn git_error_local(e: impl std::fmt::Display) -> StagedImportError {
    StagedImportError::Git(e.to_string())
}
fn convert_import_error(e: crate::ImportError) -> StagedImportError {
    match e {
        crate::ImportError::InvalidUtf8(x) => StagedImportError::InvalidUtf8(x),
        other => StagedImportError::Git(other.to_string()),
    }
}
