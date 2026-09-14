//! Git compatibility boundary for Wheat.
//!
//! The initial importer treats every Git commit as one synthetic Wheat change.
//! Its first parent is the semantic dependency; additional merge parents are
//! retained only as source provenance.

pub mod bridge;
pub mod export;
pub mod journal;
pub mod push;
mod staged_import;
pub use staged_import::{
    AtomicImportError, DEFAULT_STAGE_COMMITS, FinalizeImportError, FinalizePhase, StagedBranch,
    StagedImportError, StagedImportResult, atomic_import, finalize_import,
    finalize_import_with_observer, stage_import,
};

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use gix::bstr::ByteSlice;
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    Actor, ApplicationContext, ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditTransition,
    FileKind, FileValue, Repository, RevisionEdits, RevisionMetadata, SourceProvenance, Timestamp,
    Tree, ViewId,
    edit_capture::{FileSnapshot, capture_edit},
    guarded,
};
use wheat_store::{ContentStore, ContentStoreError};

const GIT_IMPORT_NAMESPACE: Uuid = Uuid::from_u128(0xc245_a89b_519f_5a4b_9b36_d62b_78f5_2901);

/// Resource limits for the direct Git importer.
///
/// The direct importer intentionally materializes a complete [`Repository`], so it
/// rejects source histories that exceed these limits instead of returning a partial
/// repository. `Default` is conservative and is used by [`import_repository`] and
/// [`import_open_repository`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    /// Maximum local branches examined at the Git boundary.
    pub max_branches: usize,
    /// Maximum distinct commits reachable from local branch tips.
    pub max_reachable_commits: usize,
    /// Maximum entries across all unique, materialized flat Git trees.
    pub max_flat_tree_entries: usize,
    /// Maximum UTF-8 path bytes across all unique, materialized flat Git trees.
    pub max_flat_tree_bytes: usize,
    /// Maximum number of unique content objects held before `put_many`.
    pub max_pending_content_count: usize,
    /// Maximum bytes of unique content held before `put_many`.
    pub max_pending_content_bytes: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_branches: 256,
            max_reachable_commits: 10_000,
            max_flat_tree_entries: 250_000,
            max_flat_tree_bytes: 64 * 1024 * 1024,
            max_pending_content_count: 100_000,
            max_pending_content_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportLimit {
    Branches,
    ReachableCommits,
    FlatTreeEntries,
    FlatTreeBytes,
    PendingContentCount,
    PendingContentBytes,
}

impl std::fmt::Display for ImportLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Branches => "branches",
            Self::ReachableCommits => "reachable commits",
            Self::FlatTreeEntries => "flat tree entries",
            Self::FlatTreeBytes => "flat tree bytes",
            Self::PendingContentCount => "pending content count",
            Self::PendingContentBytes => "pending content bytes",
        })
    }
}

#[derive(Default)]
struct ImportAccounting {
    flat_tree_entries: usize,
    flat_tree_bytes: usize,
    pending_content_count: usize,
    pending_content_bytes: usize,
}

impl ImportAccounting {
    fn add(
        current: &mut usize,
        limit: ImportLimit,
        amount: usize,
        maximum: usize,
    ) -> Result<(), ImportError> {
        let actual = current
            .checked_add(amount)
            .ok_or(ImportError::LimitExceeded {
                limit,
                actual: usize::MAX,
                maximum,
            })?;
        if actual > maximum {
            return Err(ImportError::LimitExceeded {
                limit,
                actual,
                maximum,
            });
        }
        *current = actual;
        Ok(())
    }

    fn tree_entry(&mut self, path_bytes: usize, limits: ImportLimits) -> Result<(), ImportError> {
        Self::add(
            &mut self.flat_tree_entries,
            ImportLimit::FlatTreeEntries,
            1,
            limits.max_flat_tree_entries,
        )?;
        Self::add(
            &mut self.flat_tree_bytes,
            ImportLimit::FlatTreeBytes,
            path_bytes,
            limits.max_flat_tree_bytes,
        )
    }

    fn content(&mut self, bytes: usize, limits: ImportLimits) -> Result<(), ImportError> {
        Self::add(
            &mut self.pending_content_count,
            ImportLimit::PendingContentCount,
            1,
            limits.max_pending_content_count,
        )?;
        Self::add(
            &mut self.pending_content_bytes,
            ImportLimit::PendingContentBytes,
            bytes,
            limits.max_pending_content_bytes,
        )
    }
}

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
    #[error("edit capture failed: {0}")]
    EditCapture(#[from] wheat_core::edit_capture::EditCaptureError),
    #[error("Wheat import failed: {0}")]
    Wheat(#[from] wheat_core::RepositoryError),
    #[error("Git metadata is not valid UTF-8: {0}")]
    InvalidUtf8(&'static str),
    #[error("Git submodule at {0} is not supported by the flat Wheat tree prototype")]
    UnsupportedSubmodule(String),
    #[error("Git commit graph contains a cycle")]
    CommitCycle,
    #[error("Git import exceeded {limit}: {actual} > {maximum}")]
    LimitExceeded {
        limit: ImportLimit,
        actual: usize,
        maximum: usize,
    },
}

/// Imports using [`ImportLimits::default`]. Use [`import_repository_with_limits`]
/// when the source is expected to exceed the documented conservative defaults.
pub fn import_repository(
    path: impl AsRef<Path>,
    content: &ContentStore,
) -> Result<ImportResult, ImportError> {
    import_repository_with_limits(path, content, ImportLimits::default())
}

pub fn import_repository_with_limits(
    path: impl AsRef<Path>,
    content: &ContentStore,
    limits: ImportLimits,
) -> Result<ImportResult, ImportError> {
    let git = gix::open(path.as_ref()).map_err(git_error)?;
    import_open_repository_with_limits(&git, content, limits)
}

/// Imports using [`ImportLimits::default`]. Use [`import_open_repository_with_limits`]
/// when the source is expected to exceed the documented conservative defaults.
pub fn import_open_repository(
    git: &gix::Repository,
    content: &ContentStore,
) -> Result<ImportResult, ImportError> {
    import_open_repository_with_limits(git, content, ImportLimits::default())
}

pub fn import_open_repository_with_limits(
    git: &gix::Repository,
    content: &ContentStore,
    limits: ImportLimits,
) -> Result<ImportResult, ImportError> {
    let profile = std::env::var_os("WHEAT_IMPORT_PROFILE").is_some();
    let started = Instant::now();
    let branches = branch_tips(git, limits)?;
    let order = commit_order(git, branches.values().copied(), limits)?;
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
    let mut accounting = ImportAccounting::default();
    let mut proposals = Vec::with_capacity(order.len());
    let mut commit_changes = BTreeMap::new();

    for oid in order {
        let commit = git.find_commit(oid).map_err(git_error)?;
        let mut parents = Vec::new();
        for parent in commit.parent_ids() {
            if parents.len() >= limits.max_reachable_commits {
                return Err(ImportError::LimitExceeded {
                    limit: ImportLimit::ReachableCommits,
                    actual: parents.len().saturating_add(1),
                    maximum: limits.max_reachable_commits,
                });
            }
            parents.push(parent.detach());
        }
        merge_count += usize::from(parents.len() > 1);
        let tree = materialize_commit_tree(
            git,
            oid,
            &mut unique_content,
            &mut tree_cache,
            &mut blob_cache,
            &mut pending_content,
            &mut accounting,
            limits,
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
                    &mut accounting,
                    limits,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let edits = edits_between(
            &parent_tree,
            &tree,
            &mut pending_content,
            &mut accounting,
            limits,
        )?;
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
            edits,
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
    let base = repository.base_view().id;
    let outcomes =
        repository.import_changes(base, base, proposals, ApplicationContext::new(content))?;
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

fn branch_tips(
    git: &gix::Repository,
    limits: ImportLimits,
) -> Result<BTreeMap<String, gix::ObjectId>, ImportError> {
    let platform = git.references().map_err(git_error)?;
    let iter = platform.local_branches().map_err(git_error)?;
    let mut branches = BTreeMap::new();
    for reference in iter {
        let actual = branches
            .len()
            .checked_add(1)
            .ok_or(ImportError::LimitExceeded {
                limit: ImportLimit::Branches,
                actual: usize::MAX,
                maximum: limits.max_branches,
            })?;
        if actual > limits.max_branches {
            return Err(ImportError::LimitExceeded {
                limit: ImportLimit::Branches,
                actual,
                maximum: limits.max_branches,
            });
        }
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
    limits: ImportLimits,
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
                    let discovered = visiting.len().checked_add(visited.len()).ok_or(
                        ImportError::LimitExceeded {
                            limit: ImportLimit::ReachableCommits,
                            actual: usize::MAX,
                            maximum: limits.max_reachable_commits,
                        },
                    )?;
                    if discovered > limits.max_reachable_commits {
                        return Err(ImportError::LimitExceeded {
                            limit: ImportLimit::ReachableCommits,
                            actual: discovered,
                            maximum: limits.max_reachable_commits,
                        });
                    }
                    let commit = git.find_commit(oid).map_err(git_error)?;
                    let mut parents = Vec::new();
                    for parent in commit.parent_ids() {
                        if parents.len() >= limits.max_reachable_commits {
                            return Err(ImportError::LimitExceeded {
                                limit: ImportLimit::ReachableCommits,
                                actual: parents.len().saturating_add(1),
                                maximum: limits.max_reachable_commits,
                            });
                        }
                        parents.push(parent.detach());
                    }
                    // Bound the explicit DFS work stack too: a wide merge graph can
                    // enqueue duplicate parents before they are marked visited.
                    let queued = pending
                        .len()
                        .checked_add(parents.len())
                        .and_then(|count| count.checked_add(1)) // Exit(oid)
                        .ok_or(ImportError::LimitExceeded {
                            limit: ImportLimit::ReachableCommits,
                            actual: usize::MAX,
                            maximum: limits.max_reachable_commits,
                        })?;
                    if queued > limits.max_reachable_commits {
                        return Err(ImportError::LimitExceeded {
                            limit: ImportLimit::ReachableCommits,
                            actual: queued,
                            maximum: limits.max_reachable_commits,
                        });
                    }
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
    blob_cache: &mut BTreeMap<(Vec<u8>, bool), ContentId>,
    pending_content: &mut BTreeMap<ContentId, Vec<u8>>,
    accounting: &mut ImportAccounting,
    limits: ImportLimits,
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
        accounting,
        limits,
    )
}

fn flatten_tree(
    tree: &gix::Tree<'_>,
    unique_content: &mut BTreeSet<ContentId>,
    tree_cache: &mut BTreeMap<Vec<u8>, Tree>,
    blob_cache: &mut BTreeMap<(Vec<u8>, bool), ContentId>,
    pending_content: &mut BTreeMap<ContentId, Vec<u8>>,
    accounting: &mut ImportAccounting,
    limits: ImportLimits,
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
                accounting,
                limits,
            )? {
                let path = format!("{name}/{path}");
                accounting.tree_entry(path.len(), limits)?;
                output.insert(path, id);
            }
        } else if entry.mode().is_commit() {
            return Err(ImportError::UnsupportedSubmodule(name.to_owned()));
        } else {
            let mode = entry.mode();
            let kind = if mode.is_link() {
                FileKind::Symlink
            } else if mode.is_executable() {
                FileKind::Executable
            } else {
                FileKind::Regular
            };
            let oid = entry.id().as_bytes().to_vec();
            // A Git blob can appear both as a symlink target (always raw) and as a
            // regular/executable file (canonical text capture), so OID alone is
            // not a sufficient cache key.
            let cache_key = (oid, kind == FileKind::Symlink);
            let id = if let Some(id) = blob_cache.get(&cache_key) {
                *id
            } else {
                let blob = entry
                    .object()
                    .map_err(git_error)?
                    .try_into_blob()
                    .map_err(git_error)?;
                let captured = capture_blob(&blob.data, kind);
                let id = ContentId::of(&captured);
                if let std::collections::btree_map::Entry::Vacant(entry) = pending_content.entry(id)
                {
                    accounting.content(captured.len(), limits)?;
                    entry.insert(captured.into_owned());
                }
                blob_cache.insert(cache_key, id);
                id
            };
            unique_content.insert(id);
            accounting.tree_entry(name.len(), limits)?;
            output.insert(name.to_owned(), FileValue { content: id, kind });
        }
    }
    tree_cache.insert(tree_oid, output.clone());
    Ok(output)
}

fn capture_blob(bytes: &[u8], kind: FileKind) -> Cow<'_, [u8]> {
    if kind == FileKind::Symlink {
        return Cow::Borrowed(bytes);
    }
    match wheat_core::text_capture::capture(bytes) {
        wheat_core::text_capture::CapturedBytes::Text(Cow::Borrowed(text)) => {
            Cow::Borrowed(text.as_bytes())
        }
        wheat_core::text_capture::CapturedBytes::Text(Cow::Owned(text)) => {
            Cow::Owned(text.into_bytes())
        }
        wheat_core::text_capture::CapturedBytes::Binary(bytes) => bytes,
    }
}

fn edits_between(
    parent: &Tree,
    current: &Tree,
    content: &mut BTreeMap<ContentId, Vec<u8>>,
    accounting: &mut ImportAccounting,
    limits: ImportLimits,
) -> Result<RevisionEdits, ImportError> {
    let paths = parent.keys().chain(current.keys()).collect::<BTreeSet<_>>();
    let mut edits = BTreeMap::new();
    for path in paths {
        let before = parent.get(path).copied();
        let after = current.get(path).copied();
        if before == after {
            continue;
        }
        let before_bytes = before.map(|value| {
            content
                .get(&value.content)
                .expect("materialized Git content is retained before edit capture")
                .as_slice()
        });
        let after_bytes = after.map(|value| {
            content
                .get(&value.content)
                .expect("materialized Git content is retained before edit capture")
                .as_slice()
        });
        let captured = capture_edit(
            path,
            before.zip(before_bytes).map(|(value, bytes)| FileSnapshot {
                kind: value.kind,
                bytes,
            }),
            after.zip(after_bytes).map(|(value, bytes)| FileSnapshot {
                kind: value.kind,
                bytes,
            }),
            None,
            guarded::Limits::default(),
        )?;
        edits.extend(captured.edits);
        for (id, bytes) in captured.content {
            if let std::collections::btree_map::Entry::Vacant(entry) = content.entry(id) {
                // Guarded capture may retain replacement fragments in addition to
                // tree blobs; charge them before copying into the pending map.
                accounting.content(bytes.len(), limits)?;
                entry.insert(bytes);
            }
        }
    }
    let added = edits.keys().copied().collect();
    Ok(RevisionEdits {
        edits,
        transition: EditTransition {
            added,
            ..EditTransition::default()
        },
    })
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
