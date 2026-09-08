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
};
use wheat_store::{
    ApplicationMapStore, CommitWriterError, ConflictSetStore, ContentSetStore, ContentStore,
    ContentStoreError, ImmutableObjectStore, MerkleTreeStore, PersistedCommitWriter,
    RepositoryReader, RevisionMapStore,
};

use crate::{
    journal::{ImportJournal, JournalError, NativeResult},
    metadata,
};

mod atomic;
mod cache;
mod finalize;
pub use atomic::{AtomicImportError, atomic_import};
pub use finalize::{
    FinalizeImportError, FinalizePhase, finalize_import, finalize_import_with_observer,
};
mod tree_diff;

// These limits keep cache memory per invocation bounded while accommodating the
// shared collection nodes seen during staged-import resume.
const CACHE_ENTRIES: usize = 16_384;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const SET_RECEIPTS: usize = 4_096;

const NAMESPACE: Uuid = Uuid::from_u128(0xc245_a89b_519f_5a4b_9b36_d62b_78f5_2901);
const SNAPSHOT_VERSION: &[u8] = b"wheat-git-staged-import-v1\0";
const DISCOVERY_BATCH: usize = 256;
const MAX_STAGE_BRANCHES: usize = 256;
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

/// Discover and stage at most `max_commits` additional commits. `None` uses
/// [`DEFAULT_STAGE_COMMITS`], never an unbounded budget. Each invocation discovers at
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
    let branches = pinned_branches(&git)?;
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
    let writer = PersistedCommitWriter::new(&batch, content);
    let empty = writer.empty_view().await?;
    batch.flush().await.map_err(CommitWriterError::from)?;
    let cached = cache::CachedObjects::new(objects, CACHE_ENTRIES, CACHE_BYTES);
    let mut receipts = ProofReceipts::new(SET_RECEIPTS);
    // A journal is only a hint: authenticate every reused object and relationship first.
    let mut after = None::<Vec<u8>>;
    loop {
        let page = journal.completed(after.as_deref(), DISCOVERY_BATCH)?;
        if page.is_empty() {
            break;
        }
        for (oid, native) in page {
            verify_mapping(
                &cached,
                content,
                &journal,
                empty,
                &oid,
                native,
                &mut receipts,
            )
            .await?;
            after = Some(oid);
        }
    }

    let budget = max_commits.unwrap_or(DEFAULT_STAGE_COMMITS);
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
        // Buffered reads are not proof of durability. Flush before verification
        // against the backing store, and only then acknowledge the journal row.
        batch.flush().await.map_err(CommitWriterError::from)?;
        verify_mapping(
            &cached,
            content,
            &journal,
            empty,
            &work.oid,
            native,
            &mut receipts,
        )
        .await?;
        journal.complete(&work.oid, native)?;
        written += 1;
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

async fn collect_tree_content<O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    root: wheat_core::TreeId,
    visible: &mut std::collections::BTreeSet<wheat_core::ContentId>,
) -> Result<(), CommitWriterError> {
    for entry in trees.read_node(root).await?.entries.values() {
        match entry {
            wheat_core::DirectoryEntry::File(value) => {
                visible.insert(value.content);
            }
            wheat_core::DirectoryEntry::Directory(child) => {
                Box::pin(collect_tree_content(trees, *child, visible)).await?;
            }
        }
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

pub(super) fn pinned_branches(
    git: &gix::Repository,
) -> Result<BTreeMap<String, gix::ObjectId>, StagedImportError> {
    let mut out = BTreeMap::new();
    let platform = git.references().map_err(git_error_local)?;
    let iter = platform.local_branches().map_err(git_error_local)?;
    for reference in iter {
        if out.len() >= MAX_STAGE_BRANCHES {
            return Err(StagedImportError::Git(format!(
                "Git import exceeded branch limit of {MAX_STAGE_BRANCHES}"
            )));
        }
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

pub(super) fn snapshot_bytes(
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

/// Complete successful set traversals are proof-local receipts. They live only
/// for one `stage_import` call and therefore cannot hide corruption encountered
/// by a later invocation.
struct ProofReceipts {
    max: usize,
    successful: HashSet<(u8, ObjectId)>,
}

impl ProofReceipts {
    fn new(max: usize) -> Self {
        Self {
            max,
            successful: HashSet::new(),
        }
    }

    fn contains(&self, kind: u8, root: ObjectId) -> bool {
        self.successful.contains(&(kind, root))
    }

    fn record(&mut self, kind: u8, root: ObjectId) {
        // Bounded proof memoization is an optimization only; when full, repeat
        // verification rather than retaining unbounded state.
        if self.successful.len() < self.max {
            self.successful.insert((kind, root));
        }
    }
}

async fn verify_mapping<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    journal: &ImportJournal,
    empty: wheat_core::ViewId,
    oid: &[u8],
    n: NativeResult,
    receipts: &mut ProofReceipts,
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
    // Authenticate all roots carried by the claimed view. In particular, walk untouched tree
    // structure instead of treating a root ID as proof that every referenced node exists.
    collect_tree_content(&MerkleTreeStore::new(objects), tree, &mut BTreeSet::new())
        .await
        .map_err(|error| bad(&error.to_string()))?;
    let required_content = reader
        .manifest(view.required_content)
        .await
        .map_err(|e| bad(&e.to_string()))?
        .content;
    authenticate_set_once(
        &ContentSetStore::new(objects),
        required_content,
        receipts,
        1,
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
    let mut target = Tree::new();
    let mut definitions = Vec::with_capacity(revision.edits.len());
    for id in &revision.edits {
        let edit = reader.edit(*id).await.map_err(|e| bad(&e.to_string()))?;
        if edit.id() != *id {
            return Err(bad("edit identity"));
        }
        let components = edit.path.split('/').map(str::to_owned).collect::<Vec<_>>();
        let value = match trees.lookup(parent.tree, &components).await {
            Ok(value) => value,
            // A file at an ancestor means this exact touched path is absent, matching the
            // staged writer's file-to-directory transition semantics.
            Err(wheat_store::MerkleTreeError::FileInPath(_)) => None,
            Err(error) => return Err(bad(&error.to_string())),
        };
        if let Some(value) = value {
            target.insert(edit.path.clone(), value);
        }
        definitions.push((*id, edit));
    }
    let replay = apply_edits(
        &target,
        definitions.iter().cloned(),
        |id| content.get(id),
        ApplicationLimits::default(),
    )
    .map_err(|_| bad("edit replay"))?;
    let mut evidence = BTreeMap::new();
    for (id, outcome) in &replay.outcomes {
        let edit = definitions
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, edit)| edit)
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
    for (_, edit) in &definitions {
        let components = edit.path.split('/').map(str::to_owned).collect::<Vec<_>>();
        if trees
            .lookup(tree, &components)
            .await
            .map_err(|e| bad(&e.to_string()))?
            != replay.tree.get(&edit.path).copied()
        {
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

impl<S: ImmutableObjectStore> SetAuthentication<wheat_core::ContentSetId>
    for ContentSetStore<'_, S>
{
    async fn authenticate(
        &self,
        root: wheat_core::ContentSetId,
    ) -> Result<(), wheat_store::CollectionError> {
        let mut cursor = self.cursor(root).await?;
        while cursor.next().await?.is_some() {}
        Ok(())
    }
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
