//! Inventory-driven verification with a per-run disk-backed membership and memo index.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::NonZeroUsize,
    sync::Mutex,
};

use async_trait::async_trait;
use lru::LruCache;

use crate::{
    ChunkWalk, ConflictSetStore, ContentSetStore, ContentStoreError, ImmutableObjectStore,
    MerkleTreeStore, ObjectStoreError, RepositoryPublicationError, RepositoryReader,
    RetainedSetStore, StoredDirectory, StoredObject,
    application_map::ApplicationCollection,
    radix::{ContextWalkStep, DiffLimits, PairedContextWalkStep, Store as RadixStore},
    revision_map::RevisionCollection,
    semantic_checks::{Checks, Membership, Retained, invalid},
    verification_scratch::{ChunkProof, RadixReceiptKind, VerificationScratch},
};
use wheat_core::*;
type Result<T> = std::result::Result<T, RepositoryPublicationError>;

/// Stable coarse boundaries within a complete repository validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryValidationPhase {
    LoadState,
    IndexRetained,
    VerifyContent,
    VerifyChanges,
    VerifyLineage,
    VerifyViewTrees,
    VerifyViewRevisionMaps,
    VerifyViewApplicationMaps,
    VerifyRevisions,
    VerifyApplications,
}

impl RepositoryValidationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoadState => "load_state",
            Self::IndexRetained => "index_retained",
            Self::VerifyContent => "verify_content",
            Self::VerifyChanges => "verify_changes",
            Self::VerifyLineage => "verify_lineage",
            Self::VerifyViewTrees => "verify_view_trees",
            Self::VerifyViewRevisionMaps => "verify_view_revision_maps",
            Self::VerifyViewApplicationMaps => "verify_view_application_maps",
            Self::VerifyRevisions => "verify_revisions",
            Self::VerifyApplications => "verify_applications",
        }
    }
}

/// A store caching the objects read through it, for one validation run. It trusts
/// nothing across runs, and its fixed bounds keep its memory independent of the
/// repository's size.
pub(crate) struct CachedObjects<'a, O> {
    inner: &'a O,
    max_entries: usize,
    max_bytes: usize,
    cache: Mutex<(LruCache<ObjectId, StoredObject>, usize)>,
}

impl<'a, O> CachedObjects<'a, O> {
    /// The cache a whole-repository validation reads through: it covers the active
    /// radix DAG of a medium repository.
    pub(crate) fn for_validation(inner: &'a O) -> Self {
        Self::new(inner, 262_144, 128 * 1024 * 1024)
    }

    fn new(inner: &'a O, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner,
            max_entries,
            max_bytes,
            cache: Mutex::new((
                LruCache::new(
                    NonZeroUsize::new(max_entries.max(1)).expect("positive cache capacity"),
                ),
                0,
            )),
        }
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> ImmutableObjectStore for CachedObjects<'_, O> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        bytes: &[u8],
    ) -> std::result::Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> std::result::Result<StoredObject, ObjectStoreError> {
        if let Some(object) = self
            .cache
            .lock()
            .expect("verification cache poisoned")
            .0
            .get(&id)
            .cloned()
        {
            return Ok(object);
        }
        let object = self.inner.get(id).await?;
        let object_bytes = object.canonical_bytes.len();
        let mut cache = self.cache.lock().expect("verification cache poisoned");
        while (cache.0.len() >= self.max_entries || cache.1 + object_bytes > self.max_bytes)
            && let Some((_id, evicted)) = cache.0.pop_lru()
        {
            cache.1 -= evicted.canonical_bytes.len();
        }
        if self.max_entries != 0 && object_bytes <= self.max_bytes {
            cache.1 += object_bytes;
            if let Some(previous) = cache.0.put(id, object.clone()) {
                cache.1 -= previous.canonical_bytes.len();
            }
        }
        Ok(object)
    }

    async fn exists_many(
        &self,
        ids: &[ObjectId],
    ) -> std::result::Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

/// A directory being authenticated: the subdirectories still to visit, the
/// greatest height of those visited plus one, and for a chunked directory what
/// proving its chunks takes once they are done.
struct AuthFrame {
    tree: TreeId,
    subdirectories: std::vec::IntoIter<TreeId>,
    height: usize,
    chunks: Option<ChunkAuth>,
}

/// A chunked directory's chunks: the entries and height below each proved before,
/// and those read now, each after its parent.
struct ChunkAuth {
    count: u64,
    root: DirectoryChunkId,
    proved: HashMap<DirectoryChunkId, (u64, usize)>,
    read: Vec<ReadChunk>,
}

struct ReadChunk {
    id: DirectoryChunkId,
    key_above: Vec<u8>,
    /// The entries in the chunk itself.
    entries: u64,
    subdirectories: Vec<TreeId>,
    children: Vec<DirectoryChunkId>,
}

/// A directory whose files are being checked against an aggregate: the entries
/// still to check, and for a chunked directory the chunks they came from, to
/// record once they all pass.
struct ContentFrame {
    tree: TreeId,
    entries: std::vec::IntoIter<DirectoryEntry>,
    chunks: Vec<DirectoryChunkId>,
}

fn subdirectories<'e>(entries: impl Iterator<Item = &'e DirectoryEntry>) -> Vec<TreeId> {
    entries
        .filter_map(|entry| match entry {
            DirectoryEntry::Directory(id) => Some(*id),
            DirectoryEntry::File(_) => None,
        })
        .collect()
}

struct Verifier<'a, 'r, O, F> {
    reader: &'r RepositoryReader<'a, O>,
    scratch: VerificationScratch,
    observer: F,
}

struct LineageGraph {
    retained: BTreeSet<ViewId>,
    children_by_parent: BTreeMap<ViewId, Vec<ViewId>>,
}

/// A view whose tree and content map are proven equal, and the next of its lineage
/// children to check against it.
struct ViewTreeFrame {
    view_id: ViewId,
    view: ViewRecord,
    depth: usize,
    next_child: usize,
}

impl<O, F> Verifier<'_, '_, O, F>
where
    O: ImmutableObjectStore,
    F: FnMut(RepositoryValidationPhase),
{
    /// The checks full and incremental validation share, with membership proven against
    /// this run's indexes.
    fn checks(&self) -> Checks<'_, '_, O, VerificationScratch> {
        Checks {
            reader: self.reader,
            membership: &self.scratch,
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn index_retained(&self, kind: Retained, root: RetainedSetId) -> Result<()> {
        let mut cursor = RetainedSetStore::new(self.reader.objects)
            .cursor(root)
            .await?;
        while let Some(id) = cursor.next().await? {
            self.scratch.insert_retained(kind.family(), id)?;
        }
        Ok(())
    }

    /// Authenticate each distinct tree node once per validation run. The receipt
    /// carries height so reuse beneath a deeper directory cannot bypass the
    /// verification depth limit. A wide directory's chunks have receipts of their
    /// own, so a new version of one reads only the chunks it changed.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn authenticate_tree(&self, root: TreeId) -> Result<()> {
        const MAX_DIRECTORY_DEPTH: usize = 1023;
        if let Some(height) = self.scratch.tree_height(root)? {
            if height > MAX_DIRECTORY_DEPTH {
                return Err(invalid("directory depth exceeds verification limit"));
            }
            return Ok(());
        }

        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![self.open_auth_frame(&trees, root).await?];
        while !stack.is_empty() {
            let child_depth = stack.len();
            let frame = stack.last_mut().expect("checked non-empty stack");
            match frame.subdirectories.next() {
                Some(child) => {
                    if let Some(child_height) = self.scratch.tree_height(child)? {
                        if child_depth
                            .checked_add(child_height)
                            .is_none_or(|depth| depth > MAX_DIRECTORY_DEPTH)
                        {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        frame.height = frame.height.max(child_height + 1);
                    } else {
                        let child = self.open_auth_frame(&trees, child).await?;
                        stack.push(child);
                    }
                }
                None => {
                    let frame = stack.pop().expect("a frame is on the stack");
                    let tree = frame.tree;
                    let (height, chunks) = self.close_auth_frame(frame)?;
                    if stack.len() + height > MAX_DIRECTORY_DEPTH {
                        return Err(invalid("directory depth exceeds verification limit"));
                    }
                    for (chunk, proof) in &chunks {
                        self.scratch.record_chunk_proof(*chunk, proof)?;
                    }
                    self.scratch.record_tree_height(tree, height)?;
                    if let Some(parent) = stack.last_mut() {
                        parent.height = parent.height.max(height + 1);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads directory `tree` to authenticate it: a chunked one's chunks except those
    /// proved before at the same key.
    async fn open_auth_frame(
        &self,
        trees: &MerkleTreeStore<'_, O>,
        tree: TreeId,
    ) -> Result<AuthFrame> {
        let (subdirectories, chunks) = match trees.read_stored_directory(tree).await? {
            StoredDirectory::Flat(node) => (subdirectories(node.entries.values()), None),
            StoredDirectory::Chunked(header) => {
                let mut subdirectories_read = Vec::new();
                let mut proved = HashMap::new();
                let mut read = Vec::new();
                let mut walk = ChunkWalk::new(&header);
                while let Some((id, key)) = walk.next_chunk() {
                    if let Some(proof) = self.scratch.chunk_proof(id)?
                        && proof.key_above == key
                    {
                        proved.insert(id, (proof.entries, proof.height));
                        continue;
                    }
                    let key_above = key.to_vec();
                    let chunk = walk.read(trees).await?;
                    let subdirectories =
                        subdirectories(chunk.entries.iter().map(|entry| &entry.entry));
                    subdirectories_read.extend_from_slice(&subdirectories);
                    read.push(ReadChunk {
                        id,
                        key_above,
                        entries: chunk.entries.len() as u64,
                        subdirectories,
                        children: chunk.children,
                    });
                }
                (
                    subdirectories_read,
                    Some(ChunkAuth {
                        count: header.count,
                        root: header.root,
                        proved,
                        read,
                    }),
                )
            }
        };
        Ok(AuthFrame {
            tree,
            subdirectories: subdirectories.into_iter(),
            height: 0,
            chunks,
        })
    }

    /// The height of the directory `frame` authenticated, whose subdirectories all
    /// have receipts now, and the proofs of the chunks it read, which its tree's
    /// count is checked against.
    fn close_auth_frame(
        &self,
        frame: AuthFrame,
    ) -> Result<(usize, Vec<(DirectoryChunkId, ChunkProof)>)> {
        let Some(chunks) = frame.chunks else {
            return Ok((frame.height, Vec::new()));
        };
        let mut below = chunks.proved;
        let mut proofs = Vec::with_capacity(chunks.read.len());
        // Each chunk was read after its parent, so in reverse its children come first.
        for chunk in chunks.read.into_iter().rev() {
            let mut entries = chunk.entries;
            let mut height = 0;
            for subdirectory in chunk.subdirectories {
                let subdirectory = self
                    .scratch
                    .tree_height(subdirectory)?
                    .ok_or_else(|| invalid("a subdirectory has no receipt"))?;
                height = height.max(subdirectory + 1);
            }
            for child in chunk.children {
                let (child_entries, child_height) = below[&child];
                entries += child_entries;
                height = height.max(child_height);
            }
            below.insert(chunk.id, (entries, height));
            proofs.push((
                chunk.id,
                ChunkProof {
                    key_above: chunk.key_above,
                    entries,
                    height,
                },
            ));
        }
        let (entries, height) = below[&chunks.root];
        if entries != chunks.count {
            return Err(invalid("a chunked tree's count does not match its entries"));
        }
        Ok((height, proofs))
    }

    /// Every file of the tree names content in the aggregate.
    #[tracing::instrument(name = "verify_tree_content", level = "debug", skip_all)]
    async fn tree(&self, root: TreeId, aggregate: ContentSetId) -> Result<()> {
        self.authenticate_tree(root).await?;
        if self.scratch.tree_semantics_verified(root, aggregate)? {
            return Ok(());
        }
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![self.open_content_frame(&trees, root, aggregate).await?];
        while let Some(frame) = stack.last_mut() {
            match frame.entries.next() {
                None => {
                    // Only memoize a subtree after every descendant passed against
                    // this exact aggregate.
                    for chunk in &frame.chunks {
                        self.scratch.record_chunk_semantics(*chunk, aggregate)?;
                    }
                    self.scratch.record_tree_semantics(frame.tree, aggregate)?;
                    stack.pop();
                }
                Some(DirectoryEntry::File(value)) => self.scratch.content(value.content).await?,
                Some(DirectoryEntry::Directory(id)) => {
                    if !self.scratch.tree_semantics_verified(id, aggregate)? {
                        let child = self.open_content_frame(&trees, id, aggregate).await?;
                        stack.push(child);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads directory `tree` to check its files: a chunked one's chunks except those
    /// checked against `aggregate` before. The tree is authenticated already.
    async fn open_content_frame(
        &self,
        trees: &MerkleTreeStore<'_, O>,
        tree: TreeId,
        aggregate: ContentSetId,
    ) -> Result<ContentFrame> {
        let mut chunks = Vec::new();
        let entries = match trees.read_stored_directory(tree).await? {
            StoredDirectory::Flat(node) => node.entries.into_values().collect(),
            StoredDirectory::Chunked(header) => {
                let mut entries = Vec::new();
                let mut walk = ChunkWalk::new(&header);
                while let Some((id, _)) = walk.next_chunk() {
                    if self.scratch.chunk_semantics_verified(id, aggregate)? {
                        continue;
                    }
                    let chunk = walk.read(trees).await?;
                    entries.extend(chunk.entries.into_iter().map(|entry| entry.entry));
                    chunks.push(id);
                }
                entries
            }
        };
        Ok(ContentFrame {
            tree,
            entries: entries.into_iter(),
            chunks,
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn verify_view_trees_by_lineage_delta(
        &self,
        state: &RepositoryStateRecord,
        aggregate: ContentSetId,
        graph: &LineageGraph,
    ) -> Result<()> {
        // The base is checked in full; each other view is checked once, against the
        // first lineage parent that reaches it, so by induction from the base every
        // view's content map equals its tree's content.
        let base = self.reader.view(state.base_view).await?;
        self.tree(base.tree, aggregate).await?;
        self.checks()
            .content_map_matches_tree(&self.scratch, base.tree, base.required_content)
            .await?;

        let mut completed = BTreeSet::from([state.base_view]);
        let mut stack = vec![ViewTreeFrame {
            view_id: state.base_view,
            view: base,
            depth: 0,
            next_child: 0,
        }];
        while let Some(ViewTreeFrame {
            view_id,
            view,
            depth,
            next_child,
        }) = stack.pop()
        {
            let children = graph
                .children_by_parent
                .get(&view_id)
                .map_or(&[][..], Vec::as_slice);
            if next_child == children.len() {
                continue;
            }
            stack.push(ViewTreeFrame {
                view_id,
                view,
                depth,
                next_child: next_child + 1,
            });
            let child_id = children[next_child];
            if !completed.insert(child_id) {
                continue;
            }
            let child = self.reader.view(child_id).await?;
            self.authenticate_tree(child.tree).await?;
            let child_depth = depth.checked_add(1).ok_or_else(|| {
                invalid("view lineage traversal depth exceeds verification limit")
            })?;
            if !self
                .checks()
                .content_map_follows_tree_diff(&view, &child)
                .await?
            {
                self.tree(child.tree, aggregate).await?;
                self.checks()
                    .content_map_matches_tree(&self.scratch, child.tree, child.required_content)
                    .await?;
            }
            stack.push(ViewTreeFrame {
                view_id: child_id,
                view: child,
                depth: child_depth,
                next_child: 0,
            });
        }
        if completed != graph.retained {
            return Err(invalid("retained view is unreachable from base lineage"));
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revision_map(&self, root: RevisionMapId, retained: RetainedSetId) -> Result<()> {
        let store = RadixStore::<_, RevisionCollection>::new(self.reader.objects);
        let mut walk = store
            .context_walk(root)
            .await
            .map_err(crate::RevisionMapError::from)?;
        while let Some(step) = walk.next().await.map_err(crate::RevisionMapError::from)? {
            match step {
                ContextWalkStep::Node { id } => {
                    if self.scratch.radix_node_verified(
                        RadixReceiptKind::RevisionMap,
                        retained.as_bytes(),
                        id.into(),
                        walk.prefix(),
                    )? {
                        walk.prune();
                    }
                }
                ContextWalkStep::Entry {
                    key: change_id,
                    value: revision_id,
                } => {
                    self.scratch
                        .retained(Retained::Revisions, revision_id.into())
                        .await?;
                    if self.reader.revision(revision_id).await?.change_id != change_id {
                        return Err(invalid("selected revision change mismatch"));
                    }
                }
                ContextWalkStep::Complete { id } => self.scratch.record_radix_node(
                    RadixReceiptKind::RevisionMap,
                    retained.as_bytes(),
                    id.into(),
                    walk.prefix(),
                )?,
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn application_map(
        &self,
        application_root: ApplicationMapId,
        revision_root: RevisionMapId,
        retained: RetainedSetId,
        conflict_root: ConflictSetId,
    ) -> Result<()> {
        self.scratch.clear_produced_conflicts()?;
        let applications = RadixStore::<_, ApplicationCollection>::new(self.reader.objects);
        let revisions = RadixStore::<_, RevisionCollection>::new(self.reader.objects);
        let mut walk = applications
            .paired_context_walk(
                application_root,
                revisions,
                revision_root,
                DiffLimits::default(),
            )
            .await
            .map_err(|_| invalid("application and revision maps differ structurally"))?;
        while let Some(step) = walk.next().await.map_err(|_| {
            invalid("application/revision map verification limit or structure failure")
        })? {
            match step {
                PairedContextWalkStep::Node {
                    left,
                    right: Some(right),
                    right_skip,
                } => {
                    if self.scratch.map_pair_verified(
                        left,
                        right,
                        walk.prefix(),
                        0,
                        right_skip,
                        retained,
                        conflict_root,
                    )? {
                        walk.prune();
                    }
                }
                PairedContextWalkStep::Node { right: None, .. } => {}
                PairedContextWalkStep::Entry {
                    left: application_id,
                    right: revision_id,
                } => {
                    self.scratch
                        .retained(Retained::Applications, application_id.into())
                        .await?;
                    let record = self.reader.application(application_id).await?;
                    if revision_id != Some(record.revision_id) {
                        return Err(invalid("selected application revision mismatch"));
                    }
                    if let ApplicationResultRecord::Conflicted { conflict, .. } = record.result {
                        if !ConflictSetStore::new(self.reader.objects)
                            .contains(conflict_root, conflict)
                            .await?
                        {
                            return Err(invalid("selected conflict missing from view"));
                        }
                        self.scratch
                            .insert_produced_conflict(conflict_root, conflict)?;
                        walk.mark_conflict();
                    }
                }
                PairedContextWalkStep::Complete {
                    left,
                    right: Some(right),
                    right_skip,
                    conflict_free: true,
                } => self.scratch.record_map_pair(
                    left,
                    right,
                    walk.prefix(),
                    0,
                    right_skip,
                    retained,
                    conflict_root,
                )?,
                PairedContextWalkStep::Complete { .. } => {}
            }
        }
        Ok(())
    }

    /// Checks I1 for `view`: the base rule for the base, and for every other view its
    /// composition, plus an edge from its composition parent. Its conflicts are those its
    /// applications produce, and its content map its tree's, both checked elsewhere. By
    /// induction from the base, every view is its lineage's applications in canonical
    /// order (I2, per application).
    ///
    /// `children` lists each view's lineage children, each list in ID order.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn verify_composition(
        &self,
        state: &RepositoryStateRecord,
        id: ViewId,
        view: &ViewRecord,
        children: &BTreeMap<ViewId, Vec<ViewId>>,
    ) -> Result<()> {
        if id == state.base_view {
            return self.checks().base(view).await;
        }
        self.checks()
            .composition(view, |parent| {
                children
                    .get(&parent)
                    .is_some_and(|children| children.binary_search(&id).is_ok())
            })
            .await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn verify_lineage_semantics(
        &self,
        state: &RepositoryStateRecord,
    ) -> Result<LineageGraph> {
        let retained = RetainedSetStore::new(self.reader.objects);
        let lineage = crate::ViewLineageMapStore::new(self.reader.objects);
        let mut retained_views = BTreeSet::new();
        let mut views = retained.cursor(state.retained_views).await?;
        while let Some(id) = views.next().await? {
            retained_views.insert(ViewId::from_digest(*id.as_bytes()));
        }

        let mut children_by_parent = BTreeMap::<ViewId, Vec<ViewId>>::new();
        let mut incoming = BTreeMap::<ViewId, usize>::new();
        for parent in &retained_views {
            let Some(children_root) = lineage
                .get(state.view_lineage, *parent)
                .await
                .map_err(|_| invalid("invalid view lineage"))?
            else {
                continue;
            };
            let mut children = retained.cursor(children_root).await?;
            while let Some(child) = children.next().await? {
                let child = ViewId::from_digest(*child.as_bytes());
                *incoming.entry(child).or_default() += 1;
                children_by_parent.entry(*parent).or_default().push(child);
            }
        }
        for children in children_by_parent.values_mut() {
            children.sort_unstable();
        }
        let mut weights = BTreeMap::new();
        for view in &retained_views {
            if *view != state.base_view && incoming.get(view).copied().unwrap_or_default() == 0 {
                return Err(invalid("retained non-base view lacks incoming lineage"));
            }
            let record = self.reader.view(*view).await?;
            self.verify_composition(state, *view, &record, &children_by_parent)
                .await?;
            weights.insert(*view, record.order.weight);
        }
        // Every edge increases the weight, so the lineage has no cycle, whatever other
        // valid states it is joined with.
        for (parent, children) in &children_by_parent {
            for child in children {
                match (weights.get(parent), weights.get(child)) {
                    (Some(parent), Some(child)) if child > parent => {}
                    (Some(_), Some(_)) => {
                        return Err(invalid("lineage edge does not increase view weight"));
                    }
                    _ => return Err(invalid("lineage edge names an unretained view")),
                }
            }
        }

        let applications = crate::ApplicationMapStore::new(self.reader.objects);
        let mut retained_applications = retained.cursor(state.retained_applications).await?;
        while let Some(id) = retained_applications.next().await? {
            let application_id = ApplicationId::from_digest(*id.as_bytes());
            let application = self.reader.application(application_id).await?;
            let revision = self.reader.revision(application.revision_id).await?;
            let parent = self.reader.view(application.target_view).await?;
            self.checks()
                .application_placement(application_id, &application, &revision, &parent)
                .await?;
            // The application's composition edge goes to the child that has its change
            // last and selects it, whose tree `verify_composition` checked. Other
            // children may select it too, through an edge spanning several changes, and
            // one whose later changes leave the tree as it was has the same tree, so the
            // tree does not tell them apart.
            let mut matches = 0usize;
            for child in children_by_parent
                .get(&application.target_view)
                .into_iter()
                .flatten()
            {
                let view = self.reader.view(*child).await?;
                if view.order.last == Some(revision.change_id)
                    && applications
                        .get(view.applications, revision.change_id)
                        .await?
                        == Some(application_id)
                {
                    matches += 1;
                }
            }
            if matches != 1 {
                return Err(invalid(
                    "retained application does not have exactly one lineage edge",
                ));
            }
        }
        Ok(LineageGraph {
            retained: retained_views,
            children_by_parent,
        })
    }

    async fn validate(&mut self, id: RepositoryStateId) -> Result<()> {
        (self.observer)(RepositoryValidationPhase::LoadState);
        let state = self.reader.state(id).await?;
        let aggregate = state.required_content;

        // Authenticate each retained inventory once and use exact disk-backed
        (self.observer)(RepositoryValidationPhase::IndexRetained);
        // membership thereafter. Repeated radix descents dominated historical verification.
        self.index_retained(Retained::Changes, state.retained_changes)
            .await?;
        self.index_retained(Retained::Views, state.retained_views)
            .await?;
        self.index_retained(Retained::Revisions, state.retained_revisions)
            .await?;
        self.index_retained(Retained::Applications, state.retained_applications)
            .await?;
        // A projected commit names the application it projects; its Git half is
        // Git's to vouch for.
        let mut projected = crate::ProjectedCommitSetStore::new(self.reader.objects)
            .cursor(state.projected_commits)
            .await?;
        while let Some(member) = projected.next().await? {
            if !self
                .scratch
                .contains_retained(Retained::Applications.family(), member.application().into())?
            {
                return Err(invalid("projected commit names a non-retained application"));
            }
        }

        // Verify and index the aggregate once. Subsequent checks are SQL membership only,
        // and raw content is read exactly once for this run.
        (self.observer)(RepositoryValidationPhase::VerifyContent);
        let mut contents = ContentSetStore::new(self.reader.objects)
            .cursor(aggregate)
            .await?;
        while let Some(id) = contents.next().await? {
            self.scratch.insert_content(id)?;
            match self.reader.content.get(id) {
                Ok(_) => {}
                Err(ContentStoreError::Missing(_)) => {
                    return Err(RepositoryPublicationError::MissingContent(id));
                }
                Err(error) => return Err(error.into()),
            }
        }

        // Change objects are addressed by content. Build the logical index exactly once,
        // before checking references, and reject every duplicate logical identity.
        (self.observer)(RepositoryValidationPhase::VerifyChanges);
        let retained = RetainedSetStore::new(self.reader.objects);
        let mut cursor = retained.cursor(state.retained_changes).await?;
        let mut retained_change_count = 0usize;
        while let Some(id) = cursor.next().await? {
            let object = ChangeObjectId::from_digest(*id.as_bytes());
            let change = self.reader.change(object).await?;
            if !self.scratch.insert_change(change.id, object)? {
                return Err(invalid("duplicate logical change id"));
            }
            retained_change_count += 1;
        }
        let mut indexed = crate::ChangeMapStore::new(self.reader.objects)
            .cursor(state.changes)
            .await?;
        let mut indexed_count = 0usize;
        while let Some((change_id, object)) = indexed.next().await? {
            let change = self.reader.change(object).await?;
            if change.id != change_id {
                return Err(invalid("change-map key does not match logical change id"));
            }
            self.scratch
                .retained(Retained::Changes, object.into())
                .await?;
            if !self.scratch.contains_change(change_id)? {
                return Err(invalid("change map differs from retained logical changes"));
            }
            indexed_count += 1;
        }
        if indexed_count != retained_change_count {
            return Err(invalid("change map omits a retained logical change"));
        }

        (self.observer)(RepositoryValidationPhase::VerifyLineage);
        self.scratch
            .retained(Retained::Views, state.base_view.into())
            .await?;
        crate::ViewLineageMapStore::new(self.reader.objects)
            .validate(
                state.view_lineage,
                state.retained_views,
                crate::ViewLineageLimits::default(),
            )
            .await
            .map_err(|_| invalid("invalid view lineage"))?;
        let lineage = self.verify_lineage_semantics(&state).await?;
        (self.observer)(RepositoryValidationPhase::VerifyViewTrees);
        self.verify_view_trees_by_lineage_delta(&state, aggregate, &lineage)
            .await?;
        (self.observer)(RepositoryValidationPhase::VerifyViewRevisionMaps);
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            let view = self
                .reader
                .view(ViewId::from_digest(*id.as_bytes()))
                .await?;
            self.revision_map(view.revisions, state.retained_revisions)
                .await?;
        }
        (self.observer)(RepositoryValidationPhase::VerifyViewApplicationMaps);
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            let view = self
                .reader
                .view(ViewId::from_digest(*id.as_bytes()))
                .await?;
            self.application_map(
                view.applications,
                view.revisions,
                state.retained_applications,
                view.unresolved_conflicts,
            )
            .await?;
            let mut conflicts = ConflictSetStore::new(self.reader.objects)
                .cursor(view.unresolved_conflicts)
                .await?;
            while let Some(id) = conflicts.next().await? {
                self.checks().conflict(id).await?;
                if !self
                    .scratch
                    .contains_produced_conflict(view.unresolved_conflicts, id)?
                {
                    return Err(invalid(
                        "view conflict not produced by selected applications",
                    ));
                }
            }
        }
        (self.observer)(RepositoryValidationPhase::VerifyRevisions);
        let mut cursor = retained.cursor(state.retained_revisions).await?;
        while let Some(id) = cursor.next().await? {
            let revision = self
                .reader
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            self.tree(revision.resulting_tree, aggregate).await?;
            self.checks().revision(&revision).await?;
            self.reader.check_combination(&revision).await?;
        }
        (self.observer)(RepositoryValidationPhase::VerifyApplications);
        let mut cursor = retained.cursor(state.retained_applications).await?;
        while let Some(id) = cursor.next().await? {
            let app = self
                .reader
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            self.tree(
                Checks::<O, VerificationScratch>::result_tree(&app),
                aggregate,
            )
            .await?;
            self.checks().application(&app).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tree_depth_tests {
    use super::*;
    use crate::LooseObjectStore;

    async fn node(
        trees: &MerkleTreeStore<'_, LooseObjectStore>,
        entries: impl IntoIterator<Item = (&str, DirectoryEntry)>,
    ) -> TreeId {
        trees
            .write_node(&DirectoryNode {
                entries: entries
                    .into_iter()
                    .map(|(name, entry)| (name.to_owned(), entry))
                    .collect(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn structural_tree_receipt_charges_cached_subtree_height() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let trees = MerkleTreeStore::new(&objects);
        let leaf = node(&trees, []).await;
        let shared = node(&trees, [("child", DirectoryEntry::Directory(leaf))]).await;
        let shallow = node(&trees, [("a", DirectoryEntry::Directory(shared))]).await;
        let mut deep = shared;
        for depth in 0..1024 {
            deep = node(&trees, [("z", DirectoryEntry::Directory(deep))]).await;
            if depth == 1023 {
                break;
            }
        }

        let content_dir = tempfile::tempdir().unwrap();
        let content = crate::ContentStore::new(content_dir.path());
        let reader = RepositoryReader::new(&objects, &content);
        let verifier = Verifier {
            reader: &reader,
            scratch: VerificationScratch::new().unwrap(),
            observer: |_| {},
        };
        verifier.authenticate_tree(shallow).await.unwrap();
        assert!(verifier.authenticate_tree(deep).await.is_err());
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod wide_directory_tests {
    //! A wide directory's chunks have receipts of their own: a new version of the
    //! directory reads the chunks it changed, and a receipt only vouches for a chunk
    //! at the key it was proved at.

    use std::{
        collections::HashSet,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{ContentStore, LooseObjectStore};

    const FILES: u32 = 1_000;

    /// An object store counting the objects read through it.
    struct Counting {
        inner: LooseObjectStore,
        reads: AtomicUsize,
    }

    impl Counting {
        fn take(&self) -> usize {
            self.reads.swap(0, Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl ImmutableObjectStore for Counting {
        async fn put_if_absent(
            &self,
            id: ObjectId,
            bytes: &[u8],
        ) -> std::result::Result<(), ObjectStoreError> {
            self.inner.put_if_absent(id, bytes).await
        }

        async fn get(&self, id: ObjectId) -> std::result::Result<StoredObject, ObjectStoreError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get(id).await
        }

        async fn exists_many(
            &self,
            ids: &[ObjectId],
        ) -> std::result::Result<Vec<bool>, ObjectStoreError> {
            self.inner.exists_many(ids).await
        }
    }

    struct Fixture {
        _objects: tempfile::TempDir,
        _content: tempfile::TempDir,
        objects: Counting,
        content: ContentStore,
    }

    impl Fixture {
        fn new() -> Self {
            let objects = tempfile::tempdir().unwrap();
            let content = tempfile::tempdir().unwrap();
            Self {
                objects: Counting {
                    inner: LooseObjectStore::new(objects.path()),
                    reads: AtomicUsize::new(0),
                },
                content: ContentStore::new(content.path()),
                _objects: objects,
                _content: content,
            }
        }

        fn trees(&self) -> MerkleTreeStore<'_, Counting> {
            MerkleTreeStore::new(&self.objects)
        }

        /// A directory of [`FILES`] files, `extra` beside them.
        async fn wide(&self, extra: impl IntoIterator<Item = (String, DirectoryEntry)>) -> TreeId {
            let mut entries: BTreeMap<String, DirectoryEntry> = (0..FILES)
                .map(|index| (format!("f{index}"), file(index)))
                .collect();
            entries.extend(extra);
            let id = self
                .trees()
                .write_node(&DirectoryNode { entries })
                .await
                .unwrap();
            self.objects.take();
            id
        }

        /// The chunks of chunked directory `id`.
        async fn chunks(&self, id: TreeId) -> HashSet<DirectoryChunkId> {
            let trees = self.trees();
            let StoredDirectory::Chunked(header) = trees.read_stored_directory(id).await.unwrap()
            else {
                panic!("{id} is not chunked");
            };
            let mut walk = ChunkWalk::new(&header);
            let mut chunks = HashSet::new();
            while let Some((chunk, _)) = walk.next_chunk() {
                chunks.insert(chunk);
                walk.read(&trees).await.unwrap();
            }
            self.objects.take();
            chunks
        }
    }

    fn file(index: u32) -> DirectoryEntry {
        DirectoryEntry::File(FileValue {
            content: ContentId::of(&index.to_le_bytes()),
            kind: FileKind::Regular,
        })
    }

    fn verifier<'a, 'r>(
        reader: &'r RepositoryReader<'a, Counting>,
    ) -> Verifier<'a, 'r, Counting, impl FnMut(RepositoryValidationPhase)> {
        Verifier {
            reader,
            scratch: VerificationScratch::new().unwrap(),
            observer: |_| {},
        }
    }

    #[tokio::test]
    async fn a_new_version_of_a_wide_directory_reads_only_its_changed_chunks() {
        let fixture = Fixture::new();
        let before = fixture.wide([]).await;
        let after = fixture.wide([("f7".to_owned(), file(FILES))]).await;
        let changed = fixture.chunks(after).await.len()
            - fixture
                .chunks(after)
                .await
                .intersection(&fixture.chunks(before).await)
                .count();
        assert!(changed < 4, "one file changes the chunks on its key path");

        let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
        let verifier = verifier(&reader);
        let aggregate = ContentSetId::from_digest([1; 32]);
        for index in 0..=FILES {
            verifier
                .scratch
                .insert_content(ContentId::of(&index.to_le_bytes()))
                .unwrap();
        }
        verifier.tree(before, aggregate).await.unwrap();
        fixture.objects.take();

        // Authentication reads the header and the changed chunks, and so does the
        // content check after it.
        verifier.tree(after, aggregate).await.unwrap();
        assert_eq!(fixture.objects.take(), 2 * (1 + changed));
        assert_eq!(verifier.scratch.tree_height(after).unwrap(), Some(0));
    }

    #[tokio::test]
    async fn a_proved_chunk_carries_the_height_below_it() {
        let fixture = Fixture::new();
        let trees = fixture.trees();
        let leaf = trees.write_node(&DirectoryNode::default()).await.unwrap();
        let middle = trees
            .write_node(&DirectoryNode {
                entries: BTreeMap::from([("leaf".to_owned(), DirectoryEntry::Directory(leaf))]),
            })
            .await
            .unwrap();
        let before = fixture
            .wide([("sub".to_owned(), DirectoryEntry::Directory(middle))])
            .await;
        // A change on another key path keeps the chunk holding `sub`.
        let after = fixture
            .wide([
                ("sub".to_owned(), DirectoryEntry::Directory(middle)),
                ("f7".to_owned(), file(FILES)),
            ])
            .await;

        let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
        let verifier = verifier(&reader);
        verifier.authenticate_tree(before).await.unwrap();
        verifier.authenticate_tree(after).await.unwrap();
        assert_eq!(verifier.scratch.tree_height(before).unwrap(), Some(2));
        assert_eq!(verifier.scratch.tree_height(after).unwrap(), Some(2));
    }

    /// `tree` with its root chunk's first two children swapped, so each is reached
    /// under the other's key.
    async fn swapped(fixture: &Fixture, tree: TreeId) -> TreeId {
        let trees = fixture.trees();
        let StoredDirectory::Chunked(header) = trees.read_stored_directory(tree).await.unwrap()
        else {
            panic!("{tree} is not chunked");
        };
        let root = fixture.objects.get(header.root.into()).await.unwrap();
        let mut node = decode_directory_chunk_node(&root.canonical_bytes).unwrap();
        let (first, second) = (node.children[0].child, node.children[1].child);
        node.children[0].child = second;
        node.children[1].child = first;
        let node = DirectoryChunkNode::new(node.prefix, node.children, node.entries).unwrap();
        fixture
            .objects
            .put_if_absent(node.id().into(), &node.canonical_bytes())
            .await
            .unwrap();
        let header = ChunkedTree::new(header.count, node.id()).unwrap();
        fixture
            .objects
            .put_if_absent(header.id().into(), &header.canonical_bytes())
            .await
            .unwrap();
        header.id()
    }

    #[tokio::test]
    async fn a_chunk_proved_under_one_key_is_refused_under_another() {
        let fixture = Fixture::new();
        let honest = fixture.wide([]).await;
        let forged = swapped(&fixture, honest).await;
        let reader = RepositoryReader::new(&fixture.objects, &fixture.content);

        assert!(verifier(&reader).authenticate_tree(forged).await.is_err());
        let verifier = verifier(&reader);
        verifier.authenticate_tree(honest).await.unwrap();
        assert!(verifier.authenticate_tree(forged).await.is_err());
    }

    #[tokio::test]
    async fn a_header_over_proved_chunks_still_has_its_count_checked() {
        let fixture = Fixture::new();
        let honest = fixture.wide([]).await;
        let trees = fixture.trees();
        let StoredDirectory::Chunked(header) = trees.read_stored_directory(honest).await.unwrap()
        else {
            panic!("{honest} is not chunked");
        };
        let miscounted = ChunkedTree::new(header.count + 1, header.root).unwrap();
        fixture
            .objects
            .put_if_absent(miscounted.id().into(), &miscounted.canonical_bytes())
            .await
            .unwrap();

        let reader = RepositoryReader::new(&fixture.objects, &fixture.content);
        let verifier = verifier(&reader);
        verifier.authenticate_tree(honest).await.unwrap();
        assert!(verifier.authenticate_tree(miscounted.id()).await.is_err());
    }
}

/// Options governing which transition invariants to enforce.
#[derive(Clone, Debug, Default)]
pub struct TransitionValidationOptions {
    /// When true, allow removal of retained entries (non-monotonic transition).
    /// When false (default), any `Removed` delta entry is rejected.
    pub allow_non_monotonic: bool,
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// Verify all retained inventories with verified reads and fresh, disk-backed scratch indexes.
    pub async fn validate_complete(&self, id: RepositoryStateId) -> Result<()> {
        self.validate_complete_with_observer(id, |_| {}).await
    }

    /// Verify completely while reporting stable coarse phase boundaries.
    #[tracing::instrument(name = "validate_complete", level = "debug", skip_all)]
    pub async fn validate_complete_with_observer<F>(
        &self,
        id: RepositoryStateId,
        observer: F,
    ) -> Result<()>
    where
        F: FnMut(RepositoryValidationPhase),
    {
        // Cache authenticated immutable objects only for this verification run.
        // This removes repeated radix-node decoding without carrying trust across calls.
        // Fixed entry bound keeps memory independent of repository size while
        // covering the active radix DAG for medium repositories. A disk-backed
        // decoded-node cache is still needed before truly large qualification.
        let cached = CachedObjects::for_validation(self.objects);
        let reader = RepositoryReader::new(&cached, self.content);
        Verifier {
            reader: &reader,
            scratch: VerificationScratch::new()?,
            observer,
        }
        .validate(id)
        .await
    }
}
