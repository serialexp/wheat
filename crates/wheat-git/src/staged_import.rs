//! Resumable import into immutable storage. This module deliberately does not publish a repository head.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::Path,
};

use gix::bstr::ByteSlice;
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    ApplicationResultRecord, AppliedEditEvidence, ByteRange, ChangeId, ChangeProposal, ObjectId,
    SourceProvenance, Tree,
    edit_application::{ApplicationLimits, ApplicationOutcome, apply_edits},
    encode_git_import_source,
};
use wheat_store::{
    ApplicationMapStore, CommitWriterError, ConflictSetStore, ContentMapStore, ContentStore,
    ContentStoreError, ImmutableObjectStore, MerkleTreeStore, PersistedCommitWriter,
    RepositoryReader, RevisionMapStore, TreePathShape,
};

use crate::{
    journal::{ImportJournal, JournalError, NativeResult},
    metadata,
};

mod atomic;
mod cache;
mod finalize;
pub use atomic::{
    AtomicImportError, AtomicImportEvent, AtomicImportPhase, atomic_import,
    atomic_import_with_observer,
};
pub use finalize::{
    FinalizeImportError, FinalizePhase, finalize_import, finalize_import_with_observer,
};
mod tree_diff;

// These limits keep cache memory per invocation bounded while accommodating the
// shared collection nodes seen during staged-import resume.
const CACHE_ENTRIES: usize = 16_384;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const PROOF_RECEIPTS: usize = 65_536;

const NAMESPACE: Uuid = Uuid::from_u128(0xc245_a89b_519f_5a4b_9b36_d62b_78f5_2901);
const DISCOVERY_BATCH: usize = 256;
const MAX_STAGE_REFS: usize = 1024;
/// The current import path materializes a complete blob and exact replacement
/// sides in memory, so keep its whole-file ceiling explicit and conservative.
/// Native streaming/chunked content storage will allow a larger repository policy
/// without turning that policy into a per-process allocation allowance.
const MAX_IMPORT_FILE_BYTES: usize = 64 * 1024 * 1024;
/// Above this line count, imports retain text as an exact file replacement
/// rather than spending guarded-authoring work proportional to a generated or
/// lock file's size. Ordinary source files remain guarded.
const MAX_IMPORT_GUARDED_LINES: usize = 10_000;

fn import_application_limits() -> ApplicationLimits {
    ApplicationLimits {
        // Exact replacement replay may authenticate the target plus both old and
        // new identities in one path group. Duplicate IDs are resolved only once.
        max_resolved_bytes: MAX_IMPORT_FILE_BYTES.saturating_mul(3),
        ..ApplicationLimits::default()
    }
}

/// Default number of commits staged by one [`stage_import`] invocation when
/// `max_commits` is `None`. Callers resume with another invocation; discovery
/// is durably journaled and is likewise bounded to one batch per invocation.
pub const DEFAULT_STAGE_COMMITS: usize = 256;

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
    /// True only when the durable discovery frontier is exhausted and no work remains.
    pub complete: bool,
    pub completed_commits: usize,
    pub pending_commits: usize,
    /// True when parent discovery remains. Call `stage_import` again before finalizing.
    pub discovery_pending: bool,
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
    #[error("edit capture failed for Git commit {commit} at {path}: {source}")]
    EditCapture {
        commit: String,
        path: String,
        #[source]
        source: wheat_core::edit_capture::EditCaptureError,
    },
    #[error(transparent)]
    Writer(#[from] CommitWriterError),
    #[error("stored native mapping for Git commit {0} is invalid: {1}")]
    InvalidMapping(String, String),
    #[error("Git metadata is not valid UTF-8: {0}")]
    InvalidUtf8(&'static str),
    #[error("Git submodule at {0} is unsupported")]
    UnsupportedSubmodule(String),
}

/// Discover and stage at most `max_commits` additional commits, capped at
/// [`DEFAULT_STAGE_COMMITS`]. `None` uses that bound. Each invocation discovers at
/// most one durable [`DISCOVERY_BATCH`] parent frontier; `complete` remains false until
/// that frontier is exhausted. Reopening with changed refs, HEAD, repository path, or
/// format is rejected.
pub async fn stage_import<O: ImmutableObjectStore>(
    git_path: impl AsRef<Path>,
    journal_path: impl AsRef<Path>,
    objects: &O,
    content: &ContentStore,
    max_commits: Option<usize>,
) -> Result<StagedImportResult, StagedImportError> {
    stage_import_inner(
        git_path.as_ref(),
        journal_path.as_ref(),
        objects,
        content,
        max_commits,
        true,
    )
    .await
}

pub(crate) async fn stage_import_deferred<O: ImmutableObjectStore>(
    git_path: &Path,
    journal_path: &Path,
    objects: &O,
    content: &ContentStore,
    max_commits: Option<usize>,
) -> Result<StagedImportResult, StagedImportError> {
    stage_import_inner(git_path, journal_path, objects, content, max_commits, false).await
}

async fn stage_import_inner<O: ImmutableObjectStore>(
    git_path: &Path,
    journal_path: &Path,
    objects: &O,
    content: &ContentStore,
    max_commits: Option<usize>,
    durable_journal: bool,
) -> Result<StagedImportResult, StagedImportError> {
    let git = gix::open(git_path).map_err(git_error_local)?;
    let branches = pinned_refs(&git)?;
    let snapshot = snapshot_bytes(&git, git_path, &branches)?;
    let mut journal = if durable_journal {
        ImportJournal::open(journal_path, &snapshot)?
    } else {
        ImportJournal::open_deferred(journal_path, &snapshot)?
    };

    for oid in branches.values() {
        add_commit(&git, &mut journal, *oid)?;
    }
    // Discover at most one durable batch. Repeating invocations eventually reaches
    // every ancestor without retaining the source graph in process memory. Fetching
    // the next missing page after each insertion makes a linear history advance by
    // a full batch, rather than just one parent per invocation.
    let mut discovered = 0usize;
    while discovered < DISCOVERY_BATCH {
        let missing = journal.missing_parents(DISCOVERY_BATCH - discovered)?;
        if missing.is_empty() {
            break;
        }
        discovered += missing.len();
        for bytes in missing {
            let oid = gix::ObjectId::from_bytes_or_panic(&bytes);
            add_commit(&git, &mut journal, oid)?;
        }
    }
    let discovery_pending = !journal.missing_parents(1)?.is_empty();

    // Writes deliberately go directly to the buffered backing store: a cache hit
    // must never make a pending write appear durable. Reads after each flush use
    // one invocation-local cache shared by all verifiers and Merkle traversals.
    let batch = wheat_store::BatchObjectStore::new(objects, 1024, 8 * 1024 * 1024);
    let writer = PersistedCommitWriter::with_limits(&batch, content, import_application_limits());
    let empty = writer.empty_view().await?;
    batch.flush().await.map_err(CommitWriterError::from)?;
    let cached = cache::CachedObjects::new(objects, CACHE_ENTRIES, CACHE_BYTES);
    let mut receipts = ProofReceipts::new(PROOF_RECEIPTS);
    let mut verified_completed = HashSet::new();
    let mut proof_work = ProofWork::default();

    // A crash after recording preparation but before promotion leaves mappings that
    // are hints only. Reauthenticate their completed frontier and then the prepared
    // group strictly against the backing store. Cold completed history is verified
    // once at final publication rather than quadratically on every bounded resume.
    let prepared = journal.prepared(DEFAULT_STAGE_COMMITS + 1)?;
    if !prepared.is_empty() {
        let mut valid = prepared.len() <= DEFAULT_STAGE_COMMITS;
        for (oid, native) in &prepared {
            if !valid {
                break;
            }
            if authenticate_completed_parents(
                &cached,
                content,
                &journal,
                empty,
                oid,
                &mut receipts,
                &mut verified_completed,
                &mut proof_work,
            )
            .await
            .is_err()
                || verify_mapping(
                    &cached,
                    content,
                    &journal,
                    empty,
                    oid,
                    *native,
                    &mut receipts,
                    true,
                    &mut proof_work,
                )
                .await
                .is_err()
            {
                valid = false;
                break;
            }
        }
        if valid {
            journal.complete_many(&prepared)?;
        } else {
            journal.reset_prepared()?;
            receipts = ProofReceipts::new(PROOF_RECEIPTS);
            verified_completed.clear();
        }
    }

    let budget = max_commits
        .unwrap_or(DEFAULT_STAGE_COMMITS)
        .min(DEFAULT_STAGE_COMMITS);
    let mut written = 0usize;
    let mut prepared = Vec::with_capacity(budget.min(DEFAULT_STAGE_COMMITS));
    if budget != 0 {
        journal.begin_preparing()?;
    }
    while written < budget {
        let Some(work) = journal.ready(1)?.into_iter().next() else {
            break;
        };
        authenticate_completed_parents(
            &cached,
            content,
            &journal,
            empty,
            &work.oid,
            &mut receipts,
            &mut verified_completed,
            &mut proof_work,
        )
        .await?;
        let oid = gix::ObjectId::from_bytes_or_panic(&work.oid);
        let commit = git.find_commit(oid).map_err(git_error_local)?;
        let parent_native = work
            .parents
            .first()
            .map(|p| journal.mapping(p))
            .transpose()?
            .flatten();
        let parent_view = parent_native.map_or(empty, |p| p.view_id);
        let edits = tree_diff::tree_edits(&git, &commit, &work.oid, content)?;
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
                    edits,
                },
            )
            .await?;
        let native = NativeResult {
            change_id,
            revision_id: persisted.revision_id,
            view_id: persisted.resulting_view,
            application_id: persisted.application_id,
        };
        journal.prepare(&work.oid, native)?;
        prepared.push((work.oid, native));
        written += 1;
    }
    if budget != 0 {
        journal.commit_preparing()?;
    }
    if !prepared.is_empty() {
        // Buffered reads are not proof of durability. One bounded flush covers the
        // staged group. Freshly written mappings were derived from authenticated
        // parents by PersistedCommitWriter; verify their top-level durable records
        // before promotion. Final publication performs the exhaustive replay/tree
        // authentication once across the whole pinned import.
        batch.flush().await.map_err(CommitWriterError::from)?;
        for (oid, native) in &prepared {
            verify_durable_mapping(&cached, content, oid, *native).await?;
        }
        journal.complete_many(&prepared)?;
    }
    let (completed_commits, pending_commits) = journal.progress()?;
    let complete = !discovery_pending && pending_commits == 0;
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
        discovery_pending,
        branches: output,
    })
}

async fn authenticate_tree_once<O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    root: wheat_core::TreeId,
    receipts: &mut ProofReceipts,
    pending: &mut Vec<(wheat_core::TreeId, bool)>,
) -> Result<(), CommitWriterError> {
    pending.clear();
    pending.push((root, false));
    while let Some((tree, visited)) = pending.pop() {
        let object_id = tree.into();
        if receipts.contains(3, object_id) {
            continue;
        }
        if visited {
            // Record only after every descendant has authenticated successfully.
            receipts.record(3, object_id);
            continue;
        }
        let node = trees.read_node(tree).await?;
        pending.push((tree, true));
        pending.extend(node.entries.values().filter_map(|entry| match entry {
            wheat_core::DirectoryEntry::Directory(child) => Some((*child, false)),
            wheat_core::DirectoryEntry::File(_) => None,
        }));
    }
    Ok(())
}

/// Authenticates each content-map node once per invocation, at the key position it
/// is reached at. A view's map shares every subtree its commit did not change with
/// its parent's, so this costs the nodes views differ in, not views × map size.
async fn authenticate_content_map_once<O: ImmutableObjectStore>(
    maps: &ContentMapStore<'_, O>,
    root: wheat_core::ContentMapId,
    receipts: &mut ProofReceipts,
    pending: &mut Vec<(wheat_core::ContentMapId, usize, bool)>,
) -> Result<(), wheat_store::ContentMapError> {
    pending.clear();
    pending.push((root, 0, false));
    while let Some((node_id, position, visited)) = pending.pop() {
        let object_id = node_id.into();
        if receipts.contains_at(1, position, object_id) {
            continue;
        }
        if visited {
            // Record only after every descendant has authenticated successfully.
            receipts.record_at(1, position, object_id);
            continue;
        }
        let node = maps.read_node_at(node_id, position).await?;
        pending.push((node_id, position, true));
        let below = position + node.prefix.len() + 1;
        pending.extend(
            node.children
                .iter()
                .map(|child| (child.child, below, false)),
        );
    }
    Ok(())
}

async fn verify_durable_mapping<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    oid: &[u8],
    mapping: NativeResult,
) -> Result<(), StagedImportError> {
    let bad = |why: &str| StagedImportError::InvalidMapping(hex(oid), why.to_owned());
    if mapping.change_id != ChangeId::from_import_identity(NAMESPACE, oid) {
        return Err(bad("change identity"));
    }
    let reader = RepositoryReader::new(objects, content);
    let revision = reader
        .revision(mapping.revision_id)
        .await
        .map_err(|error| bad(&error.to_string()))?;
    let application = reader
        .application(mapping.application_id)
        .await
        .map_err(|error| bad(&error.to_string()))?;
    let view = reader
        .view(mapping.view_id)
        .await
        .map_err(|error| bad(&error.to_string()))?;
    if revision.change_id != mapping.change_id
        || application.revision_id != mapping.revision_id
        || !matches!(
            application.result,
            ApplicationResultRecord::Clean { tree }
                if tree == revision.resulting_tree && tree == view.tree
        )
    {
        return Err(bad("durable revision/application/view relationship"));
    }
    Ok(())
}

async fn authenticate_completed_parents<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    journal: &ImportJournal,
    empty: wheat_core::ViewId,
    oid: &[u8],
    receipts: &mut ProofReceipts,
    verified: &mut HashSet<Vec<u8>>,
    work: &mut ProofWork,
) -> Result<(), StagedImportError> {
    for parent in journal.parents(oid)? {
        if verified.contains(&parent) {
            continue;
        }
        let Some(native) = journal.result(&parent)? else {
            if journal.mapping(&parent)?.is_some() {
                continue;
            }
            return Err(StagedImportError::InvalidMapping(
                hex(&parent),
                "missing completed or prepared parent".into(),
            ));
        };
        verify_mapping(
            objects, content, journal, empty, &parent, native, receipts, false, work,
        )
        .await?;
        verified.insert(parent);
    }
    Ok(())
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

/// Collect all importable refs: local branches, tags, and remote-tracking
/// branches. Each ref is peeled to its target commit OID. Annotated tags
/// (tag objects) are peeled through to the underlying commit; tags that
/// point at non-commit objects (trees, blobs) are silently skipped.
pub(super) fn pinned_refs(
    git: &gix::Repository,
) -> Result<BTreeMap<String, gix::ObjectId>, StagedImportError> {
    let mut out = BTreeMap::new();
    let platform = git.references().map_err(git_error_local)?;

    // Helper: add refs from one iterator, peeling each to a commit.
    let mut add_refs = |iter: gix::reference::iter::Iter<'_, '_>| -> Result<(), StagedImportError> {
        for reference in iter {
            if out.len() >= MAX_STAGE_REFS {
                return Err(StagedImportError::Git(format!(
                    "Git import exceeded ref limit of {MAX_STAGE_REFS}"
                )));
            }
            let mut reference = reference.map_err(git_error_local)?;
            let name = reference
                .name()
                .as_bstr()
                .to_str()
                .map_err(|_| StagedImportError::InvalidUtf8("ref name"))?
                .to_owned();
            // Peel the reference (follows symrefs) to get the target OID.
            let oid = reference.peel_to_id().map_err(git_error_local)?.detach();
            // For annotated tags the OID points at a tag object, not a
            // commit. Try to peel through to the commit; skip if the
            // target is a non-commit (e.g. a tag pointing at a tree).
            let commit_oid = match git.find_object(oid) {
                Ok(obj) => match obj.peel_to_kind(gix::object::Kind::Commit) {
                    Ok(commit) => commit.id,
                    Err(_) => continue, // skip non-commit targets
                },
                Err(_) => continue, // skip unresolvable refs
            };
            out.insert(name, commit_oid);
        }
        Ok(())
    };

    add_refs(platform.local_branches().map_err(git_error_local)?)?;
    // Re-acquire the platform for each iterator (gix consumes it).
    let platform = git.references().map_err(git_error_local)?;
    add_refs(platform.tags().map_err(git_error_local)?)?;
    let platform = git.references().map_err(git_error_local)?;
    add_refs(platform.remote_branches().map_err(git_error_local)?)?;

    Ok(out)
}

pub(super) fn snapshot_bytes(
    git: &gix::Repository,
    supplied: &Path,
    branches: &BTreeMap<String, gix::ObjectId>,
) -> Result<Vec<u8>, StagedImportError> {
    let canonical =
        std::fs::canonicalize(supplied).map_err(|e| StagedImportError::Git(e.to_string()))?;
    let head = std::fs::read(git.path().join("HEAD"))
        .map_err(|e| StagedImportError::Git(e.to_string()))?;
    encode_git_import_source(
        canonical.to_string_lossy().as_bytes(),
        &head,
        branches
            .iter()
            .map(|(name, oid)| (name.as_bytes(), oid.as_bytes())),
    )
    .map_err(|e| StagedImportError::Git(format!("Git source does not encode: {e}")))
}

/// Complete successful set traversals are proof-local receipts. They live only
/// for one `stage_import` call and therefore cannot hide corruption encountered
/// by a later invocation.
struct ProofReceipts {
    max: usize,
    /// Kind, key position (radix nodes are validated relative to it; 0 for
    /// everything else), and object.
    successful: HashSet<(u8, usize, ObjectId)>,
}

impl ProofReceipts {
    fn new(max: usize) -> Self {
        Self {
            max,
            successful: HashSet::new(),
        }
    }

    fn contains(&self, kind: u8, root: ObjectId) -> bool {
        self.contains_at(kind, 0, root)
    }

    fn record(&mut self, kind: u8, root: ObjectId) {
        self.record_at(kind, 0, root);
    }

    fn contains_at(&self, kind: u8, position: usize, id: ObjectId) -> bool {
        self.successful.contains(&(kind, position, id))
    }

    fn record_at(&mut self, kind: u8, position: usize, id: ObjectId) {
        // Bounded proof memoization is an optimization only; when full, repeat
        // verification rather than retaining unbounded state.
        if self.successful.len() < self.max {
            self.successful.insert((kind, position, id));
        }
    }
}

/// Reusable traversal stacks for one invocation's proofs.
#[derive(Default)]
struct ProofWork {
    trees: Vec<(wheat_core::TreeId, bool)>,
    /// Content-map nodes with the key position each is reached at.
    content_maps: Vec<(wheat_core::ContentMapId, usize, bool)>,
}

async fn verify_mapping<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    journal: &ImportJournal,
    empty: wheat_core::ViewId,
    oid: &[u8],
    n: NativeResult,
    receipts: &mut ProofReceipts,
    allow_prepared_parent: bool,
    work: &mut ProofWork,
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
        .map(|p| {
            if allow_prepared_parent {
                journal.mapping(p)
            } else {
                journal.result(p)
            }
        })
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
    // Authenticate every tree node, but only once per invocation. Persistent
    // path-copying shares unchanged subtrees across views; postorder receipts make
    // verification proportional to the union of nodes rather than snapshots × tree.
    authenticate_tree_once(
        &MerkleTreeStore::new(objects),
        tree,
        receipts,
        &mut work.trees,
    )
    .await
    .map_err(|error| bad(&error.to_string()))?;
    authenticate_content_map_once(
        &ContentMapStore::new(objects),
        view.required_content,
        receipts,
        &mut work.content_maps,
    )
    .await
    .map_err(|error| bad(&error.to_string()))?;
    authenticate_set_once(
        &ConflictSetStore::new(objects),
        view.unresolved_conflicts,
        receipts,
        2,
    )
    .await
    .map_err(|error| bad(&error.to_string()))?;

    // Neither the journal nor the claimed trees are authoritative. Authenticate every selected
    // definition and replay it against the authenticated parent before accepting this mapping.
    let parent = reader
        .view(expected_target)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    let trees = MerkleTreeStore::new(objects);
    let mut definitions = Vec::with_capacity(revision.edits.len());
    for id in &revision.edits {
        let edit = reader.edit(*id).await.map_err(|e| bad(&e.to_string()))?;
        if edit.id() != *id {
            return Err(bad("edit identity"));
        }
        definitions.push((*id, edit));
    }
    // Both trees are read with one shared walk over the sorted touched paths, which
    // decodes each directory once. A lookup per edit decoded a wide directory again for
    // every edit below it.
    let paths = definitions
        .iter()
        .map(|(_, edit)| edit.path.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut target = Tree::new();
    let before = trees
        .path_shapes(parent.tree, &paths)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    for (path, shape) in paths.iter().zip(before) {
        // A file at an ancestor means this exact touched path is absent, matching the
        // staged writer's file-to-directory transition semantics.
        if let TreePathShape::File(value) = shape {
            target.insert((*path).to_owned(), value);
        }
    }
    let replay = apply_edits(
        &target,
        definitions.iter().cloned(),
        |id| content.get(id),
        import_application_limits(),
    )
    .map_err(|_| bad("edit replay"))?;
    let mut evidence = BTreeMap::new();
    for (id, outcome) in &replay.outcomes {
        // `definitions` follows `revision.edits`, a `BTreeSet`, so it is sorted by ID.
        let edit = definitions
            .binary_search_by(|(candidate, _)| candidate.cmp(id))
            .map(|index| &definitions[index].1)
            .expect("replay reports selected edit");
        let ApplicationOutcome::Applied { read, write } = outcome else {
            return Err(bad("replayed edit did not apply cleanly"));
        };
        evidence.insert(
            *id,
            AppliedEditEvidence {
                target: target.get(&edit.path).copied(),
                read: read.clone().map(byte_range),
                write: write.clone().map(byte_range),
            },
        );
    }
    if evidence != application.applied {
        return Err(bad("application evidence differs from replay"));
    }
    let after = trees
        .path_shapes(tree, &paths)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    for (path, shape) in paths.iter().zip(after) {
        // Likewise a touched path below a file in the result is absent: its directory
        // became a file.
        let value = match shape {
            TreePathShape::File(value) => Some(value),
            _ => None,
        };
        if value != replay.tree.get(*path).copied() {
            return Err(bad("resulting tree differs from replay"));
        }
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

async fn authenticate_set_once<S, I>(
    store: &S,
    root: I,
    receipts: &mut ProofReceipts,
    kind: u8,
) -> Result<(), wheat_store::CollectionError>
where
    S: SetAuthentication<I>,
    I: Into<ObjectId> + Copy,
{
    let object_root = root.into();
    if receipts.contains(kind, object_root) {
        return Ok(());
    }
    store.authenticate(root).await?;
    // Record only after the cursor reached its successful end; a corrupt shared
    // node therefore fails on its first encounter and earns no receipt.
    receipts.record(kind, object_root);
    Ok(())
}

trait SetAuthentication<I> {
    async fn authenticate(&self, root: I) -> Result<(), wheat_store::CollectionError>;
}

impl<S: ImmutableObjectStore> SetAuthentication<wheat_core::ConflictSetId>
    for ConflictSetStore<'_, S>
{
    async fn authenticate(
        &self,
        root: wheat_core::ConflictSetId,
    ) -> Result<(), wheat_store::CollectionError> {
        let mut cursor = self.cursor(root).await?;
        while cursor.next().await?.is_some() {}
        Ok(())
    }
}

fn byte_range(range: std::ops::Range<usize>) -> ByteRange {
    ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use object_store::memory::InMemory;
    use wheat_core::ContentId;
    use wheat_store::{ObjectKind, ObjectStoreAdapter, ObjectStoreError, StoredObject};

    use super::*;

    struct CountingStore {
        inner: ObjectStoreAdapter,
        content_map_reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ImmutableObjectStore for CountingStore {
        async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
            let object = self.inner.get(id).await?;
            if object.kind == ObjectKind::ContentMapNode {
                self.content_map_reads.fetch_add(1, Ordering::Relaxed);
            }
            Ok(object)
        }

        async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
            self.inner.put_if_absent(id, bytes).await
        }

        async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
            self.inner.exists_many(ids).await
        }
    }

    fn content(index: u32) -> ContentId {
        ContentId::of(&index.to_le_bytes())
    }

    fn counting(name: &str) -> CountingStore {
        CountingStore {
            inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), name),
            content_map_reads: AtomicUsize::new(0),
        }
    }

    /// A second map sharing all but one path with an authenticated one reads only that
    /// path's nodes.
    #[tokio::test]
    async fn content_map_nodes_are_authenticated_once() {
        let objects = counting("shared-map");
        let maps = ContentMapStore::new(&objects);
        let base = maps
            .adjust(
                maps.empty_root().await.unwrap(),
                (0..512).map(|index| (content(index), 1)),
            )
            .await
            .unwrap();
        let next = maps.adjust(base, [(content(10_000), 1)]).await.unwrap();
        let mut receipts = ProofReceipts::new(PROOF_RECEIPTS);
        let mut pending = Vec::new();

        objects.content_map_reads.store(0, Ordering::Relaxed);
        authenticate_content_map_once(&maps, base, &mut receipts, &mut pending)
            .await
            .unwrap();
        let whole = objects.content_map_reads.load(Ordering::Relaxed);
        assert!(whole > 100, "{whole} nodes in a 512-blob map");

        objects.content_map_reads.store(0, Ordering::Relaxed);
        authenticate_content_map_once(&maps, next, &mut receipts, &mut pending)
            .await
            .unwrap();
        authenticate_content_map_once(&maps, base, &mut receipts, &mut pending)
            .await
            .unwrap();
        let changed = objects.content_map_reads.load(Ordering::Relaxed);
        // The new root and at most the one child holding the new blob.
        assert!(changed <= 2, "{changed} nodes read for a one-blob change");
    }

    /// A map with an unreadable node fails and earns no receipt, so it fails again.
    #[tokio::test]
    async fn a_failed_content_map_earns_no_receipt() {
        let objects = counting("broken-map");
        let mut nodes = Vec::new();
        let root =
            wheat_core::build_content_map((0..512).map(|index| (content(index), 1)), |node| {
                nodes.push(node)
            });
        // Store every node but one child.
        let root_id = ObjectId::from(root);
        let missing = nodes.iter().find(|node| node.id != root_id).unwrap().id;
        for node in nodes.iter().filter(|node| node.id != missing) {
            objects.put_if_absent(node.id, &node.bytes).await.unwrap();
        }
        let maps = ContentMapStore::new(&objects);
        let mut receipts = ProofReceipts::new(PROOF_RECEIPTS);
        let mut pending = Vec::new();
        for _ in 0..2 {
            assert!(
                authenticate_content_map_once(&maps, root, &mut receipts, &mut pending)
                    .await
                    .is_err()
            );
        }
        assert!(!receipts.contains_at(1, 0, root_id));
    }
}
