//! Deterministic projection of a clean, linear Wheat application history to Git.

use std::collections::{BTreeMap, BTreeSet};

use gix::bstr::BString;
use thiserror::Error;
use wheat_core::{
    Actor, ApplicationId, ApplicationResultRecord, ChangeId, DirectoryEntry, FileKind, RevisionId,
    Timestamp, TreeId, ViewId,
};
use wheat_store::{
    ApplicationMapError, ApplicationMapStore, ConflictSetStore, ContentStore, ContentStoreError,
    ImmutableObjectStore, MerkleTreeError, MerkleTreeStore, RepositoryPublicationError,
    RepositoryReader,
};

/// Hard bounds for an export. Exceeding one fails before a partial history is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportLimits {
    pub max_applications: usize,
    pub max_tree_entries: usize,
    pub max_tree_depth: usize,
    pub max_content_bytes: usize,
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
    /// An already-present Git commit to reuse. It must match either imported
    /// provenance or a caller-supplied application mapping in the selected chain.
    pub anchor: Option<gix::ObjectId>,
    /// Previously verified projection mappings held outside Wheat's canonical store.
    pub projected_anchors: BTreeMap<ApplicationId, gix::ObjectId>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportResult {
    pub tip: Option<gix::ObjectId>,
    pub applications: BTreeMap<ApplicationId, gix::ObjectId>,
    pub revisions: BTreeMap<RevisionId, gix::ObjectId>,
    pub views: BTreeMap<ViewId, gix::ObjectId>,
    pub projected: Vec<ExportedApplication>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportLimit {
    Applications,
    TreeEntries,
    TreeDepth,
    ContentBytes,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("Wheat repository read failed")]
    Repository(#[from] RepositoryPublicationError),
    #[error("application-map read failed")]
    ApplicationMap(#[from] ApplicationMapError),
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
    #[error("view {view} has {count} possible producing applications")]
    AmbiguousApplication { view: ViewId, count: usize },
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
    #[error("Git anchor {0} does not match provenance in the selected chain")]
    UnmatchedAnchor(gix::ObjectId),
    #[error(
        "projected Git anchor {anchor} has tree {actual_tree}, expected {expected_tree} for application {application}"
    )]
    MismatchedProjectedAnchor {
        application: ApplicationId,
        anchor: gix::ObjectId,
        actual_tree: gix::ObjectId,
        expected_tree: gix::ObjectId,
    },
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
/// content IDs supplied by the caller. The destination must be a bare Git
/// repository. Objects are written directly through gix; refs are left to the
/// caller so publication can use its own compare-and-swap policy.
pub async fn export_revision_history<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    selected: ViewId,
    base: ViewId,
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

    let anchor_index = if let Some(anchor) = options.anchor {
        let anchor_commit = git
            .find_commit(anchor)
            .map_err(|_| ExportError::MissingAnchor(anchor))?;
        let index = chain
            .iter()
            .position(|entry| {
                provenance_matches(&entry.revision, anchor)
                    || options.projected_anchors.get(&entry.application) == Some(&anchor)
            })
            .ok_or(ExportError::UnmatchedAnchor(anchor))?;
        if options.projected_anchors.get(&chain[index].application) == Some(&anchor) {
            let view = reader.view(chain[index].result).await?;
            let mut verification_cache = ObjectCache::default();
            let expected_tree = write_tree(
                objects,
                content,
                git,
                view.tree,
                options.limits,
                &mut verification_cache,
                0,
            )
            .await?;
            let actual_tree = anchor_commit.tree_id().map_err(git_error)?.detach();
            if actual_tree != expected_tree {
                return Err(ExportError::MismatchedProjectedAnchor {
                    application: chain[index].application,
                    anchor,
                    actual_tree,
                    expected_tree,
                });
            }
        }
        Some(index)
    } else {
        None
    };

    let mut output = ExportResult {
        tip: options.anchor,
        applications: BTreeMap::new(),
        revisions: BTreeMap::new(),
        views: BTreeMap::new(),
        projected: Vec::new(),
    };
    let mut parent = options.anchor;
    let mut object_cache = ObjectCache::default();
    let start = anchor_index.unwrap_or(usize::MAX);
    for (index, entry) in chain.into_iter().enumerate() {
        if anchor_index.is_some() && index < start {
            continue;
        }
        // An anchor in the middle of the chain cannot stand in for later imported
        // applications: they must still be projected as commits after it.
        let reused_anchor = anchor_index == Some(index);
        let oid = if reused_anchor {
            options.anchor.expect("anchor index requires anchor")
        } else {
            let view = reader.view(entry.result).await?;
            let tree = write_tree(
                objects,
                content,
                git,
                view.tree,
                options.limits,
                &mut object_cache,
                0,
            )
            .await?;
            write_commit(git, tree, parent, &entry.revision, &options.fallback)?
        };
        parent = Some(oid);
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
        });
    }
    output.tip = parent;
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
        let mut cursor = ApplicationMapStore::new(objects)
            .cursor(view.applications)
            .await?;
        let mut candidates = Vec::new();
        while let Some((change, application_id)) = cursor.next().await? {
            let application = reader.application(application_id).await?;
            if let ApplicationResultRecord::Clean { tree } = application.result
                && tree == view.tree
            {
                let target = reader.view(application.target_view).await?;
                if application_map_is_predecessor(
                    objects,
                    target.applications,
                    view.applications,
                    change,
                    application_id,
                    limits.max_applications,
                )
                .await?
                {
                    candidates.push((change, application_id, application));
                }
            }
        }
        if candidates.len() != 1 {
            if candidates.is_empty() {
                return Err(ExportError::Disconnected { selected, base });
            }
            return Err(ExportError::AmbiguousApplication {
                view: current,
                count: candidates.len(),
            });
        }
        let (change, application_id, application) = candidates.pop().expect("one candidate");
        let target = reader.view(application.target_view).await?;
        if ApplicationMapStore::new(objects)
            .get(view.applications, change)
            .await?
            != Some(application_id)
            || target.tree == view.tree && application.target_view == current
        {
            return Err(ExportError::InconsistentApplication {
                view: current,
                application: application_id,
            });
        }
        let revision = reader.revision(application.revision_id).await?;
        if revision.change_id != change {
            return Err(ExportError::InconsistentApplication {
                view: current,
                application: application_id,
            });
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

async fn application_map_is_predecessor<O: ImmutableObjectStore>(
    objects: &O,
    target_root: wheat_core::ApplicationMapId,
    result_root: wheat_core::ApplicationMapId,
    added_change: ChangeId,
    added_application: ApplicationId,
    maximum: usize,
) -> Result<bool, ExportError> {
    let maps = ApplicationMapStore::new(objects);
    if maps.get(target_root, added_change).await?.is_some() {
        return Ok(false);
    }
    let mut target = maps.cursor(target_root).await?;
    let mut result = maps.cursor(result_root).await?;
    let mut left = target.next().await?;
    let mut right = result.next().await?;
    let mut seen = 0usize;
    while left.is_some() || right.is_some() {
        seen = seen.saturating_add(1);
        if seen > maximum.saturating_add(1) {
            return Err(ExportError::LimitExceeded {
                limit: ExportLimit::Applications,
                actual: seen,
                maximum,
            });
        }
        if right == Some((added_change, added_application)) {
            right = result.next().await?;
            continue;
        }
        if left != right {
            return Ok(false);
        }
        left = target.next().await?;
        right = result.next().await?;
    }
    Ok(true)
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
}

fn write_tree<'a, O: ImmutableObjectStore>(
    objects: &'a O,
    content: &'a ContentStore,
    git: &'a gix::Repository,
    tree_id: TreeId,
    limits: ExportLimits,
    cache: &'a mut ObjectCache,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<gix::ObjectId, ExportError>> + 'a>> {
    Box::pin(async move {
        if let Some(oid) = cache.trees.get(&tree_id) {
            return Ok(*oid);
        }
        if depth > limits.max_tree_depth {
            return Err(ExportError::LimitExceeded {
                limit: ExportLimit::TreeDepth,
                actual: depth,
                maximum: limits.max_tree_depth,
            });
        }
        let node = MerkleTreeStore::new(objects).read_node(tree_id).await?;
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
            let (mode, oid) = match value {
                DirectoryEntry::Directory(child) => (
                    gix::objs::tree::EntryKind::Tree.into(),
                    write_tree(objects, content, git, child, limits, cache, depth + 1).await?,
                ),
                DirectoryEntry::File(file) => {
                    let oid = if let Some(oid) = cache.contents.get(&file.content) {
                        *oid
                    } else {
                        let bytes = content.get(file.content)?;
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
                    let mode = match file.kind {
                        FileKind::Regular => gix::objs::tree::EntryKind::Blob,
                        FileKind::Executable => gix::objs::tree::EntryKind::BlobExecutable,
                        FileKind::Symlink => gix::objs::tree::EntryKind::Link,
                    };
                    (mode.into(), oid)
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

fn write_commit(
    git: &gix::Repository,
    tree: gix::ObjectId,
    parent: Option<gix::ObjectId>,
    revision: &wheat_core::RevisionRecord,
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
    let message = if metadata.message.is_empty() {
        fallback.message.as_str()
    } else {
        metadata.message.as_str()
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
