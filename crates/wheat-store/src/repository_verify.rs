//! Inventory-driven verification with a per-run disk-backed membership and memo index.
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Mutex,
};

use async_trait::async_trait;
use lru::LruCache;

use crate::{
    ConflictSetStore, ContentSetStore, ContentStoreError, ImmutableObjectStore, MerkleTreeStore,
    ObjectStoreError, RepositoryPublicationError, RepositoryReader, RetainedSetStore, StoredObject,
    application_map::ApplicationCollection,
    radix::{ContextWalkStep, DiffLimits, PairedContextWalkStep, Store as RadixStore},
    revision_map::RevisionCollection,
    semantic_checks::{Checks, Membership, Retained, invalid},
    verification_scratch::{RadixReceiptKind, VerificationScratch},
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
    /// verification depth limit.
    async fn authenticate_tree(&self, root: TreeId) -> Result<()> {
        const MAX_DIRECTORY_DEPTH: usize = 1023;
        if let Some(height) = self.scratch.tree_height(root)? {
            if height > MAX_DIRECTORY_DEPTH {
                return Err(invalid("directory depth exceeds verification limit"));
            }
            return Ok(());
        }

        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![(
            root,
            trees.read_node(root).await?.entries.into_values(),
            0usize,
        )];
        while !stack.is_empty() {
            let child_depth = stack.len();
            let (node, frame, height) = stack.last_mut().expect("checked non-empty stack");
            match frame.next() {
                Some(DirectoryEntry::File(_)) => {}
                Some(DirectoryEntry::Directory(child)) => {
                    if let Some(child_height) = self.scratch.tree_height(child)? {
                        if child_depth
                            .checked_add(child_height)
                            .is_none_or(|depth| depth > MAX_DIRECTORY_DEPTH)
                        {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        *height = (*height).max(child_height + 1);
                    } else {
                        stack.push((
                            child,
                            trees.read_node(child).await?.entries.into_values(),
                            0,
                        ));
                    }
                }
                None => {
                    let node = *node;
                    let height = *height;
                    if stack.len() - 1 + height > MAX_DIRECTORY_DEPTH {
                        return Err(invalid("directory depth exceeds verification limit"));
                    }
                    self.scratch.record_tree_height(node, height)?;
                    stack.pop();
                    if let Some((_, _, parent_height)) = stack.last_mut() {
                        *parent_height = (*parent_height).max(height + 1);
                    }
                }
            }
        }
        Ok(())
    }

    /// Every file of the tree names content in the aggregate.
    async fn tree(&self, root: TreeId, aggregate: ContentSetId) -> Result<()> {
        self.authenticate_tree(root).await?;
        if self.scratch.tree_semantics_verified(root, aggregate)? {
            return Ok(());
        }
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![(root, trees.read_node(root).await?.entries.into_values())];
        while let Some((node, frame)) = stack.last_mut() {
            match frame.next() {
                None => {
                    // Only memoize a subtree after every descendant passed against
                    // this exact aggregate.
                    self.scratch.record_tree_semantics(*node, aggregate)?;
                    stack.pop();
                }
                Some(DirectoryEntry::File(value)) => self.scratch.content(value.content).await?,
                Some(DirectoryEntry::Directory(id)) => {
                    if !self.scratch.tree_semantics_verified(id, aggregate)? {
                        stack.push((id, trees.read_node(id).await?.entries.into_values()));
                    }
                }
            }
        }
        Ok(())
    }

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
