//! Deterministic projection of a clean, linear Wheat application history to Git.

use std::collections::{BTreeMap, BTreeSet};

use gix::bstr::BString;
use thiserror::Error;
use wheat_core::{
    Actor, ApplicationId, ApplicationResultRecord, ChangeId, ChangeMapId, DirectoryEntry, FileKind,
    ProjectedCommitSetId, RevisionId, Timestamp, TreeId, ViewId,
    message::{fold_messages, strip_title_line},
};
use wheat_store::{
    ApplicationMapDelta, ApplicationMapDeltaLimits, ApplicationMapError, ApplicationMapStore,
    ChangeMapError, ChangeMapStore, ConflictSetStore, ContentStore, ContentStoreError,
    ImmutableObjectStore, MerkleTreeError, MerkleTreeStore, ProjectedCommitSetStore,
    RepositoryPublicationError, RepositoryReader,
};

/// Hard bounds for an export. Exceeding one fails before a partial history is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportLimits {
    pub max_applications: usize,
    pub max_tree_entries: usize,
    pub max_tree_depth: usize,
    pub max_content_bytes: usize,
    /// Revisions read while collecting the notes of every projected change's checkpoints.
    pub max_note_revisions: usize,
    /// Bytes of one composed commit message.
    pub max_message_bytes: usize,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            max_applications: 10_000,
            // This counts unique directory entries across path-shared trees in one
            // projection. The qualified Gothab history exceeds 250k while remaining
            // bounded well below this explicit ceiling.
            max_tree_entries: 1_000_000,
            max_tree_depth: 128,
            // This bounds distinct content read during one projection, not peak
            // residency: blobs are written to Git and released one at a time.
            // Keep headroom for the qualified Gothab history's distinct revisions.
            max_content_bytes: 1024 * 1024 * 1024,
            // Matches the checkpoint bound of routine squash.
            max_note_revisions: 250_000,
            // A change title, description, and each revision note are each bounded at 1 MiB
            // when authored; a composed message beyond this is refused, never truncated.
            max_message_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Metadata used only when a Wheat revision omits the corresponding value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportFallbackMetadata {
    pub actor: Actor,
    pub timestamp: Timestamp,
    pub message: String,
}

impl Default for ExportFallbackMetadata {
    fn default() -> Self {
        Self {
            actor: Actor {
                name: "Wheat".into(),
                email: "wheat@localhost".into(),
            },
            timestamp: Timestamp {
                seconds: 0,
                offset_minutes: 0,
            },
            message: "Wheat revision".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExportOptions {
    pub limits: ExportLimits,
    pub fallback: ExportFallbackMetadata,
    /// Which already-present Git commit, if any, stands in for the start of the chain.
    pub anchor: ExportAnchor,
    /// The repository state's projected commits: the Git commit each pushed
    /// application projects as. An anchor found here needs no further check, since the
    /// export that recorded it checked the history below it.
    pub projected_commits: Option<ProjectedCommitSetId>,
}

/// The Git commit an export reuses instead of writing the chain up to it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExportAnchor {
    /// Write every application in the chain.
    #[default]
    None,
    /// Reuse this commit. It must match either imported provenance or a
    /// caller-supplied application mapping in the selected chain.
    Commit(gix::ObjectId),
    /// Reuse the original commit of the newest imported application in the chain,
    /// or nothing when the chain has none. The result depends only on the view, not
    /// on what a remote holds, which is what makes a tag's commit reproducible.
    NewestImported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportedApplication {
    pub change_id: ChangeId,
    pub application_id: ApplicationId,
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub resulting_view: ViewId,
    pub commit_oid: gix::ObjectId,
    pub reused_anchor: bool,
    /// The applied revision was imported from Git: its provenance names its commit.
    pub imported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportResult {
    pub tip: Option<gix::ObjectId>,
    pub applications: BTreeMap<ApplicationId, gix::ObjectId>,
    pub revisions: BTreeMap<RevisionId, gix::ObjectId>,
    pub views: BTreeMap<ViewId, gix::ObjectId>,
    pub projected: Vec<ExportedApplication>,
    pub work: ExportWork,
}

/// What writing the commits' trees read, which grows with the paths they change rather
/// than with the tree when the export starts from a projected commit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportWork {
    /// Wheat tree nodes read, the seed's included.
    pub trees_read: usize,
    /// File contents read to write as Git blobs.
    pub contents_read: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportLimit {
    Applications,
    TreeEntries,
    TreeDepth,
    ContentBytes,
    NoteRevisions,
    MessageBytes,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("Wheat repository read failed")]
    Repository(#[from] RepositoryPublicationError),
    #[error("application-map read failed")]
    ApplicationMap(#[from] ApplicationMapError),
    #[error("change-map read failed")]
    ChangeMap(#[from] ChangeMapError),
    #[error("projected change {0} is not in the supplied change map")]
    UnknownChange(ChangeId),
    #[error("conflict-set read failed")]
    ConflictSet(#[from] wheat_store::CollectionError),
    #[error("Wheat tree read failed")]
    Tree(#[from] MerkleTreeError),
    #[error("content read failed")]
    Content(#[from] ContentStoreError),
    #[error("selected history contains conflicted view {0}")]
    ConflictedView(ViewId),
    #[error("selected view {selected} cannot be traced to base view {base}")]
    Disconnected { selected: ViewId, base: ViewId },
    #[error("application chain contains a cycle at view {0}")]
    Cycle(ViewId),
    #[error("application {application} is inconsistent with view {view}")]
    InconsistentApplication {
        view: ViewId,
        application: ApplicationId,
    },
    #[error("export exceeded {limit:?}: {actual} > {maximum}")]
    LimitExceeded {
        limit: ExportLimit,
        actual: usize,
        maximum: usize,
    },
    #[error("Git anchor {0} is not present in the destination repository")]
    MissingAnchor(gix::ObjectId),
    #[error("Git anchor {0} is not the history of the selected chain")]
    UnmatchedAnchor(gix::ObjectId),
    #[error("projected-commit read failed")]
    ProjectedCommits(#[source] wheat_store::CollectionError),
    #[error("Git export failed: {0}")]
    Git(String),
}

struct ChainEntry {
    change: ChangeId,
    application: ApplicationId,
    revision: wheat_core::RevisionRecord,
    target: ViewId,
    result: ViewId,
}

/// Project the clean applications from `base` through `selected` into `git`.
///
/// Neither view is discovered from mutable repository state: both are pinned
/// content IDs supplied by the caller, as is `changes`, the repository state's
/// logical-change map used for each commit's message. The destination must be a
/// bare Git repository. Objects are written directly through gix; refs are left
/// to the caller so publication can use its own compare-and-swap policy.
///
/// One commit is written per projected change application. Its message is
/// described by [`commit_message`]; author and committer come from the applied
/// revision.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn export_revision_history<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    selected: ViewId,
    base: ViewId,
    changes: ChangeMapId,
    git: &gix::Repository,
    options: &ExportOptions,
) -> Result<ExportResult, ExportError> {
    if !git.is_bare() {
        return Err(ExportError::Git(
            "destination repository is not bare".into(),
        ));
    }
    let reader = RepositoryReader::new(objects, content);
    ensure_clean(&reader, objects, selected).await?;
    ensure_clean(&reader, objects, base).await?;
    let chain = reconstruct_chain(&reader, objects, selected, base, options.limits).await?;

    let anchor = match options.anchor {
        ExportAnchor::None => None,
        ExportAnchor::Commit(oid) => Some(oid),
        ExportAnchor::NewestImported => chain
            .iter()
            .rev()
            .find_map(|entry| imported_commit(&entry.revision).transpose())
            .transpose()?,
    };
    let projected = Projected {
        store: ProjectedCommitSetStore::new(objects),
        root: options.projected_commits,
    };
    // The newest projected commit at or below the anchor seeds the first tree written.
    let mut seed = None;
    let anchor_index = if let Some(anchor) = anchor {
        let mut commit = git
            .find_commit(anchor)
            .map_err(|_| ExportError::MissingAnchor(anchor))?;
        let projected_as_anchor = projected.applications(anchor).await?;
        let index = chain
            .iter()
            .position(|entry| {
                provenance_matches(&entry.revision, anchor)
                    || projected_as_anchor.contains(&entry.application)
            })
            .ok_or(ExportError::UnmatchedAnchor(anchor))?;
        // Reusing the anchor stands in for every application before it, so they must
        // be exactly the anchor's first-parent history. Otherwise an application
        // ordered before an imported commit would silently go unpushed. A projected
        // commit's history was checked by the export that recorded it.
        for position in (0..=index).rev() {
            let entry = &chain[position];
            if projected
                .applications(commit.id)
                .await?
                .contains(&entry.application)
            {
                seed = Some(TreeSeed {
                    wheat: reader.view(entry.result).await?.tree,
                    git: commit.tree_id().map_err(git_error)?.detach(),
                });
                break;
            }
            if !provenance_matches(&entry.revision, commit.id) {
                return Err(ExportError::UnmatchedAnchor(anchor));
            }
            if position > 0 {
                let parent = commit
                    .parent_ids()
                    .next()
                    .ok_or(ExportError::UnmatchedAnchor(anchor))?
                    .detach();
                commit = git
                    .find_commit(parent)
                    .map_err(|_| ExportError::UnmatchedAnchor(anchor))?;
            }
        }
        Some(index)
    } else {
        None
    };

    let mut output = ExportResult {
        tip: anchor,
        applications: BTreeMap::new(),
        revisions: BTreeMap::new(),
        views: BTreeMap::new(),
        projected: Vec::new(),
        work: ExportWork::default(),
    };
    let mut parent = anchor;
    let mut object_cache = ObjectCache::default();
    let mut messages = MessageBuilder::new(objects, changes, options.limits);
    let start = anchor_index.unwrap_or(usize::MAX);
    for (index, entry) in chain.into_iter().enumerate() {
        if anchor_index.is_some() && index < start {
            continue;
        }
        // An anchor in the middle of the chain cannot stand in for later imported
        // applications: they must still be projected as commits after it.
        let reused_anchor = anchor_index == Some(index);
        let oid = if reused_anchor {
            anchor.expect("anchor index requires anchor")
        } else {
            let view = reader.view(entry.result).await?;
            let tree = write_tree(
                objects,
                content,
                git,
                view.tree,
                seed,
                options.limits,
                &mut object_cache,
                0,
            )
            .await?;
            seed = Some(TreeSeed {
                wheat: view.tree,
                git: tree,
            });
            let message = messages
                .commit_message(&reader, entry.change, &entry.revision)
                .await?;
            write_commit(
                git,
                tree,
                parent,
                &entry.revision,
                &message,
                &options.fallback,
            )?
        };
        parent = Some(oid);
        let imported = imported_commit(&entry.revision)?.is_some();
        output.applications.insert(entry.application, oid);
        output.revisions.insert(entry.revision.id(), oid);
        output.views.insert(entry.result, oid);
        output.projected.push(ExportedApplication {
            change_id: entry.change,
            application_id: entry.application,
            revision_id: entry.revision.id(),
            target_view: entry.target,
            resulting_view: entry.result,
            commit_oid: oid,
            reused_anchor,
            imported,
        });
    }
    output.tip = parent;
    output.work = object_cache.work;
    Ok(output)
}

async fn ensure_clean<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    objects: &O,
    view_id: ViewId,
) -> Result<(), ExportError> {
    let view = reader.view(view_id).await?;
    let mut conflicts = ConflictSetStore::new(objects)
        .cursor(view.unresolved_conflicts)
        .await?;
    if conflicts.next().await?.is_some() {
        return Err(ExportError::ConflictedView(view_id));
    }
    Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn reconstruct_chain<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    objects: &O,
    selected: ViewId,
    base: ViewId,
    limits: ExportLimits,
) -> Result<Vec<ChainEntry>, ExportError> {
    let mut reverse = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = selected;
    while current != base {
        if !seen.insert(current) {
            return Err(ExportError::Cycle(current));
        }
        if reverse.len() >= limits.max_applications {
            return Err(ExportError::LimitExceeded {
                limit: ExportLimit::Applications,
                actual: reverse.len() + 1,
                maximum: limits.max_applications,
            });
        }
        ensure_clean(reader, objects, current).await?;
        let view = reader.view(current).await?;
        // A view is the target of its last change's application with that application
        // added, so the chain steps from each view to that target.
        let Some(change) = view.order.last else {
            return Err(ExportError::Disconnected { selected, base });
        };
        let application_id = ApplicationMapStore::new(objects)
            .get(view.applications, change)
            .await?
            .ok_or(ExportError::Disconnected { selected, base })?;
        let inconsistent = || ExportError::InconsistentApplication {
            view: current,
            application: application_id,
        };
        let application = reader.application(application_id).await?;
        if application.result != (ApplicationResultRecord::Clean { tree: view.tree }) {
            return Err(inconsistent());
        }
        let target = reader.view(application.target_view).await?;
        if !application_map_is_predecessor(
            objects,
            target.applications,
            view.applications,
            change,
            application_id,
        )
        .await?
        {
            return Err(inconsistent());
        }
        let revision = reader.revision(application.revision_id).await?;
        if revision.change_id != change {
            return Err(inconsistent());
        }
        reverse.push(ChainEntry {
            change,
            application: application_id,
            revision,
            target: application.target_view,
            result: current,
        });
        current = application.target_view;
    }
    reverse.reverse();
    Ok(reverse)
}

/// Whether `result_root` is `target_root` with only `added_change` added, as
/// `added_application`. The diff stops at a second difference, so it reads the path to
/// that change rather than either map.
async fn application_map_is_predecessor<O: ImmutableObjectStore>(
    objects: &O,
    target_root: wheat_core::ApplicationMapId,
    result_root: wheat_core::ApplicationMapId,
    added_change: ChangeId,
    added_application: ApplicationId,
) -> Result<bool, ExportError> {
    let delta = ApplicationMapStore::new(objects)
        .delta(
            target_root,
            result_root,
            ApplicationMapDeltaLimits {
                max_changes: 1,
                ..ApplicationMapDeltaLimits::default()
            },
        )
        .await;
    match delta {
        Err(ApplicationMapError::DeltaChangeLimitExceeded { .. }) => Ok(false),
        delta => Ok(delta?
            == [ApplicationMapDelta::Added {
                change: added_change,
                application: added_application,
            }]),
    }
}

/// The original Git commit of an imported revision, or `None` for a native one.
fn imported_commit(
    revision: &wheat_core::RevisionRecord,
) -> Result<Option<gix::ObjectId>, ExportError> {
    let Some(provenance) = revision
        .metadata
        .provenance
        .as_ref()
        .filter(|p| p.system == "git-sha1")
    else {
        return Ok(None);
    };
    gix::hash::oid::try_from_bytes(&provenance.object_id)
        .map(|oid| Some(oid.to_owned()))
        .map_err(|_| {
            ExportError::Git(format!(
                "revision {} has a malformed git-sha1 provenance ID",
                revision.id()
            ))
        })
}

/// The state's projected commits, when the caller has them.
struct Projected<'a, O> {
    store: ProjectedCommitSetStore<'a, O>,
    root: Option<ProjectedCommitSetId>,
}

impl<O: ImmutableObjectStore> Projected<'_, O> {
    /// The applications that project as `commit`.
    async fn applications(&self, commit: gix::ObjectId) -> Result<Vec<ApplicationId>, ExportError> {
        let (Some(root), Some(commit)) = (self.root, commit_key(commit)) else {
            return Ok(Vec::new());
        };
        self.store
            .applications(root, commit)
            .await
            .map_err(ExportError::ProjectedCommits)
    }
}

/// The projected-commit key of `commit`; only SHA-1 commits are recorded.
pub(crate) fn commit_key(
    commit: gix::ObjectId,
) -> Option<[u8; wheat_core::ProjectedCommit::COMMIT_LEN]> {
    commit.as_bytes().try_into().ok()
}

fn provenance_matches(revision: &wheat_core::RevisionRecord, oid: gix::ObjectId) -> bool {
    revision
        .metadata
        .provenance
        .as_ref()
        .is_some_and(|p| p.system == "git-sha1" && p.object_id == oid.as_bytes())
}

#[derive(Default)]
struct ObjectCache {
    trees: BTreeMap<TreeId, gix::ObjectId>,
    contents: BTreeMap<wheat_core::ContentId, gix::ObjectId>,
    entries: usize,
    content_bytes: usize,
    work: ExportWork,
}

/// A Wheat tree and the Git tree an export wrote for it. Every subtree and file the
/// two Wheat trees share keeps the Git entry written for it, so writing a tree reads
/// only the paths that differ from the seed's.
///
/// Only an export's own output is a seed: an imported commit's tree is Git's, and
/// import normalises text and modes, so its entries need not be what an export
/// writes.
#[derive(Clone, Copy)]
struct TreeSeed {
    wheat: TreeId,
    git: gix::ObjectId,
}

/// The seed's entries for one directory: the Wheat node, and the Git tree's entries
/// by name.
struct SeedEntries {
    wheat: BTreeMap<String, DirectoryEntry>,
    git: BTreeMap<Vec<u8>, (gix::objs::tree::EntryMode, gix::ObjectId)>,
}

impl SeedEntries {
    async fn read<O: ImmutableObjectStore>(
        objects: &O,
        git: &gix::Repository,
        seed: TreeSeed,
    ) -> Result<Self, ExportError> {
        let wheat = MerkleTreeStore::new(objects)
            .read_node(seed.wheat)
            .await?
            .entries;
        let tree = git.find_tree(seed.git).map_err(git_error)?;
        let git = tree
            .decode()
            .map_err(git_error)?
            .entries
            .iter()
            .map(|entry| (entry.filename.to_vec(), (entry.mode, entry.oid.to_owned())))
            .collect();
        Ok(Self { wheat, git })
    }

    /// The seed's Wheat entry and Git entry named `name`.
    fn get(
        &self,
        name: &str,
    ) -> Option<(&DirectoryEntry, gix::objs::tree::EntryMode, gix::ObjectId)> {
        let wheat = self.wheat.get(name)?;
        let (mode, oid) = self.git.get(name.as_bytes())?;
        Some((wheat, *mode, *oid))
    }
}

#[allow(clippy::too_many_arguments)]
fn write_tree<'a, O: ImmutableObjectStore>(
    objects: &'a O,
    content: &'a ContentStore,
    git: &'a gix::Repository,
    tree_id: TreeId,
    seed: Option<TreeSeed>,
    limits: ExportLimits,
    cache: &'a mut ObjectCache,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<gix::ObjectId, ExportError>> + 'a>> {
    Box::pin(async move {
        if let Some(oid) = cache.trees.get(&tree_id) {
            return Ok(*oid);
        }
        if let Some(seed) = seed
            && seed.wheat == tree_id
        {
            cache.trees.insert(tree_id, seed.git);
            return Ok(seed.git);
        }
        if depth > limits.max_tree_depth {
            return Err(ExportError::LimitExceeded {
                limit: ExportLimit::TreeDepth,
                actual: depth,
                maximum: limits.max_tree_depth,
            });
        }
        let node = MerkleTreeStore::new(objects).read_node(tree_id).await?;
        cache.work.trees_read += 1;
        let seeded = match seed {
            Some(seed) => {
                cache.work.trees_read += 1;
                Some(SeedEntries::read(objects, git, seed).await?)
            }
            None => None,
        };
        let mut entries = Vec::with_capacity(node.entries.len());
        for (name, value) in node.entries {
            cache.entries = cache.entries.saturating_add(1);
            if cache.entries > limits.max_tree_entries {
                return Err(ExportError::LimitExceeded {
                    limit: ExportLimit::TreeEntries,
                    actual: cache.entries,
                    maximum: limits.max_tree_entries,
                });
            }
            let seeded = seeded.as_ref().and_then(|seeded| seeded.get(&name));
            let (mode, oid) = match value {
                DirectoryEntry::Directory(child) => {
                    let seed = match seeded {
                        Some((DirectoryEntry::Directory(wheat), mode, git)) if mode.is_tree() => {
                            Some(TreeSeed { wheat: *wheat, git })
                        }
                        _ => None,
                    };
                    (
                        gix::objs::tree::EntryKind::Tree.into(),
                        write_tree(objects, content, git, child, seed, limits, cache, depth + 1)
                            .await?,
                    )
                }
                DirectoryEntry::File(file) => {
                    let mode: gix::objs::tree::EntryMode = match file.kind {
                        FileKind::Regular => gix::objs::tree::EntryKind::Blob,
                        FileKind::Executable => gix::objs::tree::EntryKind::BlobExecutable,
                        FileKind::Symlink => gix::objs::tree::EntryKind::Link,
                    }
                    .into();
                    let oid = if let Some((DirectoryEntry::File(seeded), seeded_mode, oid)) = seeded
                        && *seeded == file
                        && seeded_mode == mode
                    {
                        oid
                    } else if let Some(oid) = cache.contents.get(&file.content) {
                        *oid
                    } else {
                        let bytes = content.get(file.content)?;
                        cache.work.contents_read += 1;
                        cache.content_bytes = cache.content_bytes.saturating_add(bytes.len());
                        if cache.content_bytes > limits.max_content_bytes {
                            return Err(ExportError::LimitExceeded {
                                limit: ExportLimit::ContentBytes,
                                actual: cache.content_bytes,
                                maximum: limits.max_content_bytes,
                            });
                        }
                        let oid = git
                            .write_object(&gix::objs::Blob { data: bytes })
                            .map_err(git_error)?
                            .detach();
                        cache.contents.insert(file.content, oid);
                        oid
                    };
                    (mode, oid)
                }
            };
            entries.push(gix::objs::tree::Entry {
                mode,
                filename: BString::from(name),
                oid,
            });
        }
        entries.sort();
        let oid = git
            .write_object(&gix::objs::Tree { entries })
            .map(|id| id.into())
            .map_err(git_error)?;
        cache.trees.insert(tree_id, oid);
        Ok(oid)
    })
}

/// Builds commit messages for one export, bounding the checkpoint reads across all of it.
struct MessageBuilder<'a, O> {
    changes: ChangeMapStore<'a, O>,
    root: ChangeMapId,
    limits: ExportLimits,
    note_reads: usize,
}

impl<'a, O: ImmutableObjectStore> MessageBuilder<'a, O> {
    fn new(objects: &'a O, root: ChangeMapId, limits: ExportLimits) -> Self {
        Self {
            changes: ChangeMapStore::new(objects),
            root,
            limits,
            note_reads: 0,
        }
    }

    /// The commit message for `revision`, the applied revision of `change`.
    ///
    /// A revision imported from another system (it carries source provenance) keeps its
    /// message byte-for-byte, so projecting imported history reproduces the original commits.
    ///
    /// Otherwise the commit describes the logical change: its title, then its description,
    /// then the note of every revision of the change from its first checkpoint through
    /// `revision`, folded by [`fold_messages`]. A note's leading line equal to the title is
    /// dropped (the first checkpoint records the whole `-m` text), empty and repeated parts are
    /// skipped, and `Co-authored-by` trailers are gathered into one block at the end. The
    /// message ends with a newline, like Git's own. An empty result selects the caller's
    /// fallback message.
    async fn commit_message(
        &mut self,
        reader: &RepositoryReader<'_, O>,
        change: ChangeId,
        revision: &wheat_core::RevisionRecord,
    ) -> Result<String, ExportError> {
        if revision.metadata.provenance.is_some() {
            return Ok(revision.metadata.message.clone());
        }
        let object = self
            .changes
            .get(self.root, change)
            .await?
            .ok_or(ExportError::UnknownChange(change))?;
        let record = reader.change(object).await?;
        if record.id != change {
            return Err(ExportError::UnknownChange(change));
        }

        // Walk back from the applied revision, keeping only notes that say something. The raw
        // notes are bounded like the composed message so a long chain cannot grow unbounded.
        let mut notes = Vec::new();
        let mut note_bytes = 0usize;
        let mut keep = |message: String| -> Result<(), ExportError> {
            if message.trim().is_empty() {
                return Ok(());
            }
            note_bytes = note_bytes.saturating_add(message.len());
            if note_bytes > self.limits.max_message_bytes {
                return Err(ExportError::LimitExceeded {
                    limit: ExportLimit::MessageBytes,
                    actual: note_bytes,
                    maximum: self.limits.max_message_bytes,
                });
            }
            notes.push(message);
            Ok(())
        };
        keep(revision.metadata.message.clone())?;
        let mut at = revision.predecessor;
        while let Some(id) = at {
            self.note_reads = self.note_reads.saturating_add(1);
            if self.note_reads > self.limits.max_note_revisions {
                return Err(ExportError::LimitExceeded {
                    limit: ExportLimit::NoteRevisions,
                    actual: self.note_reads,
                    maximum: self.limits.max_note_revisions,
                });
            }
            let predecessor = reader.revision(id).await?;
            at = predecessor.predecessor;
            keep(predecessor.metadata.message)?;
        }
        notes.reverse();

        let title = record.metadata.title.as_str();
        let mut message = fold_messages(
            [title, record.metadata.description.as_str()]
                .into_iter()
                .chain(notes.iter().map(|note| strip_title_line(note, title))),
        );
        if !message.is_empty() {
            message.push('\n');
        }
        if message.len() > self.limits.max_message_bytes {
            return Err(ExportError::LimitExceeded {
                limit: ExportLimit::MessageBytes,
                actual: message.len(),
                maximum: self.limits.max_message_bytes,
            });
        }
        Ok(message)
    }
}

fn write_commit(
    git: &gix::Repository,
    tree: gix::ObjectId,
    parent: Option<gix::ObjectId>,
    revision: &wheat_core::RevisionRecord,
    message: &str,
    fallback: &ExportFallbackMetadata,
) -> Result<gix::ObjectId, ExportError> {
    let metadata = &revision.metadata;
    let author = signature(
        if metadata.author.name.is_empty() && metadata.author.email.is_empty() {
            &fallback.actor
        } else {
            &metadata.author
        },
        if metadata.authored_at == Timestamp::default() {
            fallback.timestamp
        } else {
            metadata.authored_at
        },
    );
    let committer = signature(
        if metadata.committer.name.is_empty() && metadata.committer.email.is_empty() {
            &fallback.actor
        } else {
            &metadata.committer
        },
        if metadata.committed_at == Timestamp::default() {
            fallback.timestamp
        } else {
            metadata.committed_at
        },
    );
    let message = if message.is_empty() {
        fallback.message.as_str()
    } else {
        message
    };
    git.write_object(&gix::objs::Commit {
        tree,
        parents: parent.into_iter().collect(),
        author,
        committer,
        encoding: None,
        message: BString::from(message),
        extra_headers: Vec::new(),
    })
    .map(|id| id.into())
    .map_err(git_error)
}

/// Write the annotated tag object `name` for `commit` from a label's stored annotation.
///
/// Every byte comes from the annotation, the name and the commit: no clock, config or
/// signing key is read, so the tag has the same ID wherever it is written. The message
/// ends with a newline, as `git tag -a` writes it.
pub fn write_tag(
    git: &gix::Repository,
    commit: gix::ObjectId,
    name: &str,
    annotation: &wheat_core::LabelAnnotation,
) -> Result<gix::ObjectId, ExportError> {
    let mut message = annotation.message.clone();
    if !message.ends_with('\n') {
        message.push('\n');
    }
    git.write_object(&gix::objs::Tag {
        target: commit,
        target_kind: gix::objs::Kind::Commit,
        name: BString::from(name),
        tagger: Some(signature(&annotation.tagger, annotation.tagged_at)),
        message: BString::from(message),
        signature: None,
    })
    .map(|id| id.into())
    .map_err(git_error)
}

fn signature(actor: &Actor, timestamp: Timestamp) -> gix::actor::Signature {
    gix::actor::Signature {
        name: BString::from(actor.name.as_str()),
        email: BString::from(actor.email.as_str()),
        time: gix::date::Time {
            seconds: timestamp.seconds,
            offset: i32::from(timestamp.offset_minutes) * 60,
        },
    }
}

fn git_error(error: impl std::fmt::Display) -> ExportError {
    ExportError::Git(error.to_string())
}
