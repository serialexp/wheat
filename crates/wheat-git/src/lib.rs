//! Git compatibility boundary for Wheat.
//!
//! The initial importer treats every Git commit as one synthetic Wheat change.
//! Its first parent is the semantic dependency; additional merge parents are
//! retained only as source provenance.

pub mod journal;
mod staged_import;
pub use staged_import::{
    FinalizeImportError, StagedBranch, StagedImportError, StagedImportResult, finalize_import,
    stage_import,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use gix::bstr::ByteSlice;
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    Actor, ChangeId, ChangeMetadata, ChangeProposal, ContentId, FileEdit, Repository,
    RevisionMetadata, SourceProvenance, Timestamp, Transformation, Tree, ViewId,
};
use wheat_store::{ContentStore, ContentStoreError};

const GIT_IMPORT_NAMESPACE: Uuid = Uuid::from_u128(0xc245_a89b_519f_5a4b_9b36_d62b_78f5_2901);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedCommit {
    pub change_id: ChangeId,
    pub revision_id: wheat_core::RevisionId,
    pub resulting_view: ViewId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedBranch {
    pub name: String,
    pub tip: ImportedCommit,
}

#[derive(Clone, Debug)]
pub struct ImportResult {
    pub repository: Repository,
    pub branches: BTreeMap<String, ImportedBranch>,
    pub commits: BTreeMap<Vec<u8>, ImportedCommit>,
    pub commit_count: usize,
    pub merge_count: usize,
    pub unique_content: usize,
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("Git import failed: {0}")]
    Git(String),
    #[error("content retention failed")]
    Content(#[from] ContentStoreError),
    #[error("Wheat import failed: {0}")]
    Wheat(#[from] wheat_core::RepositoryError),
    #[error("Git metadata is not valid UTF-8: {0}")]
    InvalidUtf8(&'static str),
    #[error("Git submodule at {0} is not supported by the flat Wheat tree prototype")]
    UnsupportedSubmodule(String),
    #[error("Git commit graph contains a cycle")]
    CommitCycle,
}

pub fn import_repository(
    path: impl AsRef<Path>,
    content: &ContentStore,
) -> Result<ImportResult, ImportError> {
    let git = gix::open(path.as_ref()).map_err(git_error)?;
    import_open_repository(&git, content)
}

pub fn import_open_repository(
    git: &gix::Repository,
    content: &ContentStore,
) -> Result<ImportResult, ImportError> {
    let profile = std::env::var_os("WHEAT_IMPORT_PROFILE").is_some();
    let started = Instant::now();
    let branches = branch_tips(git)?;
    let order = commit_order(git, branches.values().copied())?;
    if profile {
        eprintln!("graph_ms={:.3}", started.elapsed().as_secs_f64() * 1_000.0);
    }
    let prepare_started = Instant::now();
    let mut repository = Repository::new(Tree::new());
    let mut imported = BTreeMap::<Vec<u8>, ImportedCommit>::new();
    let mut merge_count = 0;
    let mut unique_content = BTreeSet::new();
    let mut tree_cache = BTreeMap::new();
    let mut blob_cache = BTreeMap::new();
    let mut pending_content = BTreeMap::new();
    let mut proposals = Vec::with_capacity(order.len());
    let mut commit_changes = BTreeMap::new();

    for oid in order {
        let commit = git.find_commit(oid).map_err(git_error)?;
        let parents = commit
            .parent_ids()
            .map(|id| id.detach())
            .collect::<Vec<_>>();
        merge_count += usize::from(parents.len() > 1);
        let tree = materialize_commit_tree(
            git,
            oid,
            &mut unique_content,
            &mut tree_cache,
            &mut blob_cache,
            &mut pending_content,
        )?;
        let parent_tree = parents
            .first()
            .map(|parent| {
                materialize_commit_tree(
                    git,
                    *parent,
                    &mut unique_content,
                    &mut tree_cache,
                    &mut blob_cache,
                    &mut pending_content,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let transformation = transformation_between(&parent_tree, &tree);
        let oid_bytes = oid.as_bytes().to_vec();
        let change_id = ChangeId::from_import_identity(GIT_IMPORT_NAMESPACE, &oid_bytes);
        let dependency = parents
            .first()
            .map(|parent| {
                *commit_changes
                    .get(parent.as_bytes())
                    .expect("parents are prepared before children")
            })
            .into_iter();
        let metadata = metadata(&commit, &oid_bytes, &parents)?;
        proposals.push(ChangeProposal {
            id: change_id,
            metadata: metadata.0,
            dependencies: dependency.collect(),
            revision_metadata: metadata.1,
            transformation,
        });
        commit_changes.insert(oid_bytes, change_id);
    }

    content.put_many(pending_content, 16)?;
    if profile {
        eprintln!(
            "prepare_ms={:.3}",
            prepare_started.elapsed().as_secs_f64() * 1_000.0
        );
    }
    let materialize_started = Instant::now();
    let outcomes = repository.import_changes(proposals)?;
    if profile {
        eprintln!(
            "materialize_ms={:.3}",
            materialize_started.elapsed().as_secs_f64() * 1_000.0
        );
    }
    for (oid, change_id) in commit_changes {
        let outcome = &outcomes[&change_id];
        imported.insert(
            oid,
            ImportedCommit {
                change_id,
                revision_id: outcome.revision_id,
                resulting_view: outcome.resulting_view,
            },
        );
    }

    let mut imported_branches = BTreeMap::new();
    for (name, oid) in branches {
        let tip = imported
            .get(oid.as_bytes())
            .expect("branch tip was imported")
            .clone();
        imported_branches.insert(name.clone(), ImportedBranch { name, tip });
    }
    Ok(ImportResult {
        commit_count: imported.len(),
        merge_count,
        unique_content: unique_content.len(),
        repository,
        branches: imported_branches,
        commits: imported,
    })
}

fn branch_tips(git: &gix::Repository) -> Result<BTreeMap<String, gix::ObjectId>, ImportError> {
    let platform = git.references().map_err(git_error)?;
    let iter = platform.local_branches().map_err(git_error)?;
    let mut branches = BTreeMap::new();
    for reference in iter {
        let mut reference = reference.map_err(git_error)?;
        let name = reference
            .name()
            .shorten()
            .to_str()
            .map_err(|_| ImportError::InvalidUtf8("branch name"))?
            .to_owned();
        let oid = reference.peel_to_id().map_err(git_error)?.detach();
        branches.insert(name, oid);
    }
    Ok(branches)
}

fn commit_order(
    git: &gix::Repository,
    tips: impl IntoIterator<Item = gix::ObjectId>,
) -> Result<Vec<gix::ObjectId>, ImportError> {
    enum Visit {
        Enter(gix::ObjectId),
        Exit(gix::ObjectId),
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    for tip in tips {
        let mut pending = vec![Visit::Enter(tip)];
        while let Some(visit) = pending.pop() {
            match visit {
                Visit::Enter(oid) => {
                    let key = oid.as_bytes().to_vec();
                    if visited.contains(&key) {
                        continue;
                    }
                    if !visiting.insert(key) {
                        return Err(ImportError::CommitCycle);
                    }
                    let commit = git.find_commit(oid).map_err(git_error)?;
                    let parents = commit
                        .parent_ids()
                        .map(|parent| parent.detach())
                        .collect::<Vec<_>>();
                    pending.push(Visit::Exit(oid));
                    pending.extend(parents.into_iter().rev().map(Visit::Enter));
                }
                Visit::Exit(oid) => {
                    let key = oid.as_bytes().to_vec();
                    visiting.remove(&key);
                    if visited.insert(key) {
                        order.push(oid);
                    }
                }
            }
        }
    }
    Ok(order)
}

fn materialize_commit_tree(
    git: &gix::Repository,
    commit: gix::ObjectId,
    unique_content: &mut BTreeSet<ContentId>,
    tree_cache: &mut BTreeMap<Vec<u8>, Tree>,
    blob_cache: &mut BTreeMap<Vec<u8>, ContentId>,
    pending_content: &mut BTreeMap<ContentId, Vec<u8>>,
) -> Result<Tree, ImportError> {
    let tree = git
        .find_commit(commit)
        .map_err(git_error)?
        .tree()
        .map_err(git_error)?;
    let key = tree.id().as_bytes().to_vec();
    if let Some(cached) = tree_cache.get(&key) {
        return Ok(cached.clone());
    }
    flatten_tree(
        &tree,
        unique_content,
        tree_cache,
        blob_cache,
        pending_content,
    )
}

fn flatten_tree(
    tree: &gix::Tree<'_>,
    unique_content: &mut BTreeSet<ContentId>,
    tree_cache: &mut BTreeMap<Vec<u8>, Tree>,
    blob_cache: &mut BTreeMap<Vec<u8>, ContentId>,
    pending_content: &mut BTreeMap<ContentId, Vec<u8>>,
) -> Result<Tree, ImportError> {
    let tree_oid = tree.id().as_bytes().to_vec();
    if let Some(cached) = tree_cache.get(&tree_oid) {
        return Ok(cached.clone());
    }

    let mut output = Tree::new();
    for entry in tree.iter() {
        let entry = entry.map_err(git_error)?;
        let name = entry
            .filename()
            .to_str()
            .map_err(|_| ImportError::InvalidUtf8("tree path"))?;
        if entry.mode().is_tree() {
            let child = entry
                .object()
                .map_err(git_error)?
                .try_into_tree()
                .map_err(git_error)?;
            for (path, id) in flatten_tree(
                &child,
                unique_content,
                tree_cache,
                blob_cache,
                pending_content,
            )? {
                output.insert(format!("{name}/{path}"), id);
            }
        } else if entry.mode().is_commit() {
            return Err(ImportError::UnsupportedSubmodule(name.to_owned()));
        } else {
            let oid = entry.id().as_bytes().to_vec();
            let id = if let Some(id) = blob_cache.get(&oid) {
                *id
            } else {
                let blob = entry
                    .object()
                    .map_err(git_error)?
                    .try_into_blob()
                    .map_err(git_error)?;
                let id = ContentId::of(&blob.data);
                pending_content
                    .entry(id)
                    .or_insert_with(|| blob.data.clone());
                blob_cache.insert(oid, id);
                id
            };
            unique_content.insert(id);
            output.insert(name.to_owned(), id);
        }
    }
    tree_cache.insert(tree_oid, output.clone());
    Ok(output)
}

fn transformation_between(parent: &Tree, current: &Tree) -> Transformation {
    let paths = parent.keys().chain(current.keys()).collect::<BTreeSet<_>>();
    Transformation {
        edits: paths
            .into_iter()
            .filter_map(|path| {
                let expected = parent.get(path).copied();
                let replacement = current.get(path).copied();
                (expected != replacement).then(|| FileEdit {
                    path: path.clone(),
                    expected,
                    replacement,
                })
            })
            .collect(),
    }
}

fn metadata(
    commit: &gix::Commit<'_>,
    oid: &[u8],
    parents: &[gix::ObjectId],
) -> Result<(ChangeMetadata, RevisionMetadata), ImportError> {
    let message = commit.message_raw().map_err(git_error)?;
    let message = message
        .to_str()
        .map_err(|_| ImportError::InvalidUtf8("commit message"))?
        .to_owned();
    let author = commit.author().map_err(git_error)?;
    let committer = commit.committer().map_err(git_error)?;
    let provenance = SourceProvenance {
        system: "git-sha1".into(),
        object_id: oid.to_vec(),
        parent_object_ids: parents
            .iter()
            .map(|parent| parent.as_bytes().to_vec())
            .collect(),
    };
    let title = message.lines().next().unwrap_or_default().to_owned();
    let description = message
        .split_once('\n')
        .map_or_else(String::new, |(_, rest)| rest.trim_start().to_owned());
    let author_actor = actor(author)?;
    let committer_actor = actor(committer)?;
    let authored_at = timestamp(author)?;
    let committed_at = timestamp(committer)?;
    Ok((
        ChangeMetadata {
            title,
            description,
            creator: author_actor.clone(),
            created_at: authored_at,
            provenance: Some(provenance.clone()),
        },
        RevisionMetadata {
            message,
            author: author_actor,
            authored_at,
            committer: committer_actor,
            committed_at,
            provenance: Some(provenance),
        },
    ))
}

fn actor(signature: gix::actor::SignatureRef<'_>) -> Result<Actor, ImportError> {
    Ok(Actor {
        name: signature
            .name
            .to_str()
            .map_err(|_| ImportError::InvalidUtf8("actor name"))?
            .to_owned(),
        email: signature
            .email
            .to_str()
            .map_err(|_| ImportError::InvalidUtf8("actor email"))?
            .to_owned(),
    })
}

fn timestamp(signature: gix::actor::SignatureRef<'_>) -> Result<Timestamp, ImportError> {
    let time = signature.time().map_err(git_error)?;
    Ok(Timestamp {
        seconds: time.seconds,
        offset_minutes: i16::try_from(time.offset / 60)
            .map_err(|_| ImportError::Git("timestamp offset exceeds i16".into()))?,
    })
}

fn git_error(error: impl std::fmt::Display) -> ImportError {
    ImportError::Git(error.to_string())
}
