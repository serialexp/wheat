//! Inventory-driven verification with a per-run disk-backed membership and memo index.
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Mutex,
};

use async_trait::async_trait;
use lru::LruCache;

use crate::{
    ApplicationMapStore, ConflictSetStore, ContentMapDelta, ContentMapError, ContentMapStore,
    ContentSetStore, ContentStoreError, ImmutableObjectStore, MerkleTreeStore, ObjectStoreError,
    RepositoryPublicationError, RepositoryReader, RetainedSetStore, RevisionMapStore, StoredObject,
    TreeDiffLimits,
    application_map::ApplicationCollection,
    radix::{ContextWalkStep, DiffLimits, PairedContextWalkStep, Store as RadixStore},
    revision_map::RevisionCollection,
    verification_scratch::{RadixReceiptKind, VerificationScratch},
};
use wheat_core::*;
type Result<T> = std::result::Result<T, RepositoryPublicationError>;
const RETAINED_CHANGES: u8 = 1;
const RETAINED_VIEWS: u8 = 2;
const RETAINED_REVISIONS: u8 = 3;
const RETAINED_APPLICATIONS: u8 = 4;

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

fn invalid(message: &'static str) -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(message)
}

fn tree_delta_limit(error: &crate::MerkleTreeError) -> bool {
    matches!(
        error,
        crate::MerkleTreeError::ChangeLimitExceeded { .. }
            | crate::MerkleTreeError::NodeLimitExceeded { .. }
            | crate::MerkleTreeError::EntryLimitExceeded { .. }
            | crate::MerkleTreeError::PathBytesExceeded { .. }
            | crate::MerkleTreeError::DepthExceeded { .. }
            | crate::MerkleTreeError::NodeBytesExceeded { .. }
            | crate::MerkleTreeError::BufferedEntriesExceeded { .. }
    )
}

fn content_delta_limit(error: &ContentMapError) -> bool {
    matches!(
        error,
        ContentMapError::DeltaChangeLimitExceeded { .. }
            | ContentMapError::DeltaNodeLimitExceeded { .. }
            | ContentMapError::DeltaPendingLimitExceeded { .. }
    )
}

/// Whether a touched leaf occurs strictly below `path`. The ordered range avoids
/// scanning every edit for each directory encountered during verification.
fn has_touched_descendant(touched: &BTreeSet<String>, path: &str) -> bool {
    let prefix = format!("{path}/");
    touched
        .range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

struct CachedObjects<'a, O> {
    inner: &'a O,
    max_entries: usize,
    max_bytes: usize,
    cache: Mutex<(LruCache<ObjectId, StoredObject>, usize)>,
}

impl<'a, O> CachedObjects<'a, O> {
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

/// A revision's edits replayed against their target, with what the replay read so callers
/// need not read it again.
struct Replay {
    definitions: BTreeMap<EditId, EditRecord>,
    /// The file values the edits' paths hold in the target; a path crossing a file is
    /// absent, as the commit writer records it.
    target: Tree,
    result: wheat_core::edit_application::ApplicationResult,
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
    fn retained(&self, family: u8, id: ObjectId) -> Result<()> {
        if !self.scratch.contains_retained(family, id)? {
            return Err(invalid("reference not retained"));
        }
        Ok(())
    }

    async fn index_retained(&self, family: u8, root: RetainedSetId) -> Result<()> {
        let mut cursor = RetainedSetStore::new(self.reader.objects)
            .cursor(root)
            .await?;
        while let Some(id) = cursor.next().await? {
            self.scratch.insert_retained(family, id)?;
        }
        Ok(())
    }

    fn retained_change(&self, change: ChangeId) -> Result<()> {
        if !self.scratch.contains_change(change)? {
            return Err(invalid("logical change not retained"));
        }
        Ok(())
    }

    fn required(&self, id: ContentId) -> Result<()> {
        if !self.scratch.contains_content(id)? {
            return Err(invalid("content absent from aggregate manifest"));
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
                Some(DirectoryEntry::File(value)) => self.required(value.content)?,
                Some(DirectoryEntry::Directory(id)) => {
                    if !self.scratch.tree_semantics_verified(id, aggregate)? {
                        stack.push((id, trees.read_node(id).await?.entries.into_values()));
                    }
                }
            }
        }
        Ok(())
    }

    /// The view's content map counts exactly the tree entries naming each blob (I7).
    /// Reads the whole tree and map: the fallback when an edge can't be checked by diff.
    async fn content_map_matches_tree(&self, tree: TreeId, map: ContentMapId) -> Result<()> {
        self.scratch.clear_tree_content_counts()?;
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut contents = trees.content_cursor(tree, Default::default()).await?;
        while let Some(file) = contents.next().await? {
            self.scratch.adjust_tree_content_count(file.content, 1)?;
        }
        let mut entries = ContentMapStore::new(self.reader.objects)
            .cursor(map)
            .await?;
        let mut count = 0u64;
        while let Some((content, named)) = entries.next().await? {
            if self.scratch.tree_content_count(content)? != u64::from(named) {
                return Err(invalid("view content map differs from its tree"));
            }
            count += 1;
        }
        if count != self.scratch.tree_content_entries()? {
            return Err(invalid("view content map differs from its tree"));
        }
        Ok(())
    }

    /// Checks a lineage edge whose parent map is already proven equal to the parent's
    /// tree: the child's map must differ from the parent's by exactly the tree diff, so
    /// it equals the child's tree too. Returns false when a diff exceeds its limits and
    /// the edge needs [`Self::content_map_matches_tree`] instead.
    async fn content_map_follows_tree_diff(
        &self,
        parent: &ViewRecord,
        child: &ViewRecord,
    ) -> Result<bool> {
        let mut net = BTreeMap::<ContentId, i64>::new();
        let mut diff = MerkleTreeStore::new(self.reader.objects).diff_cursor(
            parent.tree,
            child.tree,
            TreeDiffLimits::default(),
        );
        loop {
            let change = match diff.next().await {
                Ok(Some(change)) => change,
                Ok(None) => break,
                Err(error) if tree_delta_limit(&error) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if let Some(old) = change.old {
                *net.entry(old.content).or_default() -= 1;
            }
            if let Some(new) = change.new {
                self.required(new.content)?;
                *net.entry(new.content).or_default() += 1;
            }
        }
        net.retain(|_, change| *change != 0);
        let changes = match ContentMapStore::new(self.reader.objects)
            .delta(
                parent.required_content,
                child.required_content,
                Default::default(),
            )
            .await
        {
            Ok(changes) => changes,
            Err(error) if content_delta_limit(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if changes.len() != net.len() {
            return Err(invalid("view content map differs from its tree"));
        }
        for change in changes {
            let (content, change) = match change {
                ContentMapDelta::Added { content, count } => (content, i64::from(count)),
                ContentMapDelta::Changed { content, old, new } => {
                    (content, i64::from(new) - i64::from(old))
                }
                ContentMapDelta::Removed { content, count } => (content, -i64::from(count)),
            };
            if net.get(&content) != Some(&change) {
                return Err(invalid("view content map differs from its tree"));
            }
        }
        Ok(true)
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
        self.content_map_matches_tree(base.tree, base.required_content)
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
            if !self.content_map_follows_tree_diff(&view, &child).await? {
                self.tree(child.tree, aggregate).await?;
                self.content_map_matches_tree(child.tree, child.required_content)
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
                    self.retained(RETAINED_REVISIONS, revision_id.into())?;
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
                    self.retained(RETAINED_APPLICATIONS, application_id.into())?;
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

    async fn verify_untouched(
        &self,
        target_root: TreeId,
        result_root: TreeId,
        touched: &BTreeSet<String>,
    ) -> Result<()> {
        let trees = MerkleTreeStore::new(self.reader.objects);
        // `None` is an empty subtree. This matters when an exact edit creates the
        // first leaf below a directory, or removes the last one: Merkle trees do
        // not retain those otherwise-empty directories.
        let mut stack = vec![(String::new(), Some(target_root), Some(result_root), 0usize)];
        while let Some((prefix, target_id, result_id, depth)) = stack.pop() {
            if target_id == result_id {
                continue;
            }
            let target = match target_id {
                Some(id) => Some(trees.read_node(id).await?),
                None => None,
            };
            let result = match result_id {
                Some(id) => Some(trees.read_node(id).await?),
                None => None,
            };
            let names = target
                .as_ref()
                .into_iter()
                .flat_map(|node| node.entries.keys())
                .chain(
                    result
                        .as_ref()
                        .into_iter()
                        .flat_map(|node| node.entries.keys()),
                )
                .collect::<BTreeSet<_>>();
            for name in names {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let left = target.as_ref().and_then(|node| node.entries.get(name));
                let right = result.as_ref().and_then(|node| node.entries.get(name));
                let exact = touched.contains(&path);
                let below_touched = has_touched_descendant(touched, &path);
                match (left, right) {
                    (
                        Some(DirectoryEntry::Directory(left)),
                        Some(DirectoryEntry::Directory(right)),
                    ) => {
                        if left != right {
                            if !below_touched {
                                return Err(invalid("result changes an untouched path"));
                            }
                            if depth >= 1024 {
                                return Err(invalid("directory depth exceeds verification limit"));
                            }
                            stack.push((path, Some(*left), Some(*right), depth + 1));
                        }
                    }
                    // A missing directory is an empty subtree, rather than a
                    // structural mismatch. Walk its extant peer and ensure every
                    // changed leaf remains beneath an exact edit.
                    (Some(DirectoryEntry::Directory(left)), None) => {
                        if !below_touched {
                            return Err(invalid("result changes an untouched path"));
                        }
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, Some(*left), None, depth + 1));
                    }
                    (None, Some(DirectoryEntry::Directory(right))) => {
                        if !below_touched {
                            return Err(invalid("result changes an untouched path"));
                        }
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, None, Some(*right), depth + 1));
                    }
                    // The exact edit owns a file/directory boundary, but not the
                    // directory's contents. Compare that directory against an
                    // empty peer so a fabricated sibling still fails below.
                    (Some(DirectoryEntry::File(_)), Some(DirectoryEntry::Directory(right)))
                        if exact =>
                    {
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, None, Some(*right), depth + 1));
                    }
                    (Some(DirectoryEntry::Directory(left)), Some(DirectoryEntry::File(_)))
                        if exact =>
                    {
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, Some(*left), None, depth + 1));
                    }
                    // Descendant edits cannot replace an untouched file boundary.
                    (Some(DirectoryEntry::File(_)), Some(DirectoryEntry::Directory(_)))
                    | (Some(DirectoryEntry::Directory(_)), Some(DirectoryEntry::File(_))) => {
                        return Err(invalid("result changes untouched tree structure"));
                    }
                    _ if left != right && !exact => {
                        return Err(invalid("result changes an untouched path"));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn replay(
        &self,
        target_root: TreeId,
        result_root: TreeId,
        edit_ids: &BTreeSet<EditId>,
    ) -> Result<Replay> {
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut definitions = Vec::with_capacity(edit_ids.len());
        for id in edit_ids {
            definitions.push((*id, self.reader.edit(*id).await?));
        }
        let touched: BTreeSet<String> = definitions
            .iter()
            .map(|(_, edit)| edit.path.clone())
            .collect();
        // The commit writer treats a file encountered before a requested
        // descendant as an absent leaf. A simultaneous boundary deletion
        // can then replace that file with the requested directory.
        let paths = touched.iter().map(String::as_str).collect::<Vec<_>>();
        let shapes = trees.path_shapes(target_root, &paths).await?;
        let mut target = Tree::new();
        for (path, shape) in paths.into_iter().zip(shapes) {
            if let crate::merkle_tree::TreePathShape::File(value) = shape {
                target.insert(path.to_owned(), value);
            }
        }
        self.verify_untouched(target_root, result_root, &touched)
            .await?;
        let limits = wheat_core::edit_application::ApplicationLimits {
            allow_already_satisfied: true,
            ..Default::default()
        };
        let replay = wheat_core::edit_application::apply_edits(
            &target,
            definitions.iter().cloned(),
            |id| self.reader.content.get(id),
            limits,
        )
        .map_err(|_| invalid("edit replay failed"))?;
        // `touched` is sorted, so one traversal checks every claimed value. A touched
        // path below a file in the result is absent: its directory became a file.
        let paths = touched.iter().map(String::as_str).collect::<Vec<_>>();
        let claimed = trees.path_shapes(result_root, &paths).await?;
        for (path, shape) in paths.into_iter().zip(claimed) {
            let claimed = match shape {
                crate::merkle_tree::TreePathShape::File(value) => Some(value),
                _ => None,
            };
            if claimed != replay.tree.get(path).copied() {
                return Err(invalid("claimed result differs from semantic replay"));
            }
        }
        Ok(Replay {
            definitions: definitions.into_iter().collect(),
            target,
            result: replay,
        })
    }

    /// Checks `view`'s size, last change and weight against its composition parent: the
    /// target of its last change's application, with that change's revision applied.
    /// The base has none. With every view's maps those of its composition parent plus
    /// its last change, this proves each view's order from its maps, one view at a time.
    async fn verify_view_order(
        &self,
        state: &RepositoryStateRecord,
        id: ViewId,
        view: &ViewRecord,
    ) -> Result<()> {
        if id == state.base_view {
            return if view.order == ViewOrder::BASE {
                Ok(())
            } else {
                Err(invalid("base view has changes in its order"))
            };
        }
        let change = view
            .order
            .last
            .ok_or_else(|| invalid("non-base view has no last change"))?;
        let revision_id = RevisionMapStore::new(self.reader.objects)
            .get(view.revisions, change)
            .await?
            .ok_or_else(|| invalid("view's last change is not in its revision map"))?;
        let application_id = ApplicationMapStore::new(self.reader.objects)
            .get(view.applications, change)
            .await?
            .ok_or_else(|| invalid("view's last change has no application"))?;
        let application = self.reader.application(application_id).await?;
        if application.revision_id != revision_id {
            return Err(invalid("view's last application is of another revision"));
        }
        let revision = self.reader.revision(revision_id).await?;
        let parent = self.reader.view(application.target_view).await?;
        if parent.order.then(change, revision.generation) != Some(view.order) {
            return Err(invalid("view order does not follow its composition parent"));
        }
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
        let mut weights = BTreeMap::new();
        for view in &retained_views {
            if *view != state.base_view && incoming.get(view).copied().unwrap_or_default() == 0 {
                return Err(invalid("retained non-base view lacks incoming lineage"));
            }
            let record = self.reader.view(*view).await?;
            self.verify_view_order(state, *view, &record).await?;
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

        let applications = ApplicationMapStore::new(self.reader.objects);
        let mut retained_applications = retained.cursor(state.retained_applications).await?;
        while let Some(id) = retained_applications.next().await? {
            let application_id = ApplicationId::from_digest(*id.as_bytes());
            let application = self.reader.application(application_id).await?;
            let revision = self.reader.revision(application.revision_id).await?;
            let parent = self.reader.view(application.target_view).await?;
            if applications
                .get(parent.applications, revision.change_id)
                .await?
                == Some(application_id)
            {
                return Err(invalid("application target already selects application"));
            }
            let mut matches = 0usize;
            for child in children_by_parent
                .get(&application.target_view)
                .into_iter()
                .flatten()
            {
                let view = self.reader.view(*child).await?;
                let result_tree = match application.result {
                    ApplicationResultRecord::Clean { tree } => tree,
                    ApplicationResultRecord::Conflicted { partial_tree, .. } => partial_tree,
                };
                if view.tree == result_tree
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

    async fn verify_conflict(&self, id: ConflictId, _state: &RepositoryStateRecord) -> Result<()> {
        let conflict = self.reader.conflict(id).await?;
        self.retained(RETAINED_VIEWS, conflict.base_view.into())?;
        for evidence in conflict.edits.values() {
            if let Some(value) = evidence.target {
                self.required(value.content)?;
            }
            if let EditFailure::Interference { with } = &evidence.reason {
                if with.is_empty() {
                    return Err(invalid("empty edit interference evidence"));
                }
                for edit in with {
                    self.reader.edit(*edit).await?;
                }
            }
        }
        Ok(())
    }

    async fn validate(&mut self, id: RepositoryStateId) -> Result<()> {
        (self.observer)(RepositoryValidationPhase::LoadState);
        let state = self.reader.state(id).await?;
        let aggregate = state.required_content;

        // Authenticate each retained inventory once and use exact disk-backed
        (self.observer)(RepositoryValidationPhase::IndexRetained);
        // membership thereafter. Repeated radix descents dominated historical verification.
        self.index_retained(RETAINED_CHANGES, state.retained_changes)
            .await?;
        self.index_retained(RETAINED_VIEWS, state.retained_views)
            .await?;
        self.index_retained(RETAINED_REVISIONS, state.retained_revisions)
            .await?;
        self.index_retained(RETAINED_APPLICATIONS, state.retained_applications)
            .await?;

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
            self.retained(RETAINED_CHANGES, object.into())?;
            if !self.scratch.contains_change(change_id)? {
                return Err(invalid("change map differs from retained logical changes"));
            }
            indexed_count += 1;
        }
        if indexed_count != retained_change_count {
            return Err(invalid("change map omits a retained logical change"));
        }

        (self.observer)(RepositoryValidationPhase::VerifyLineage);
        self.retained(RETAINED_VIEWS, state.base_view.into())?;
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
                self.verify_conflict(id, &state).await?;
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
            self.retained(RETAINED_VIEWS, revision.authored_against.into())?;
            self.retained_change(revision.change_id)?;
            for dependency in &revision.dependencies {
                self.retained_change(*dependency)?;
            }
            let mut parent_generations = Vec::new();
            for id in revision.predecessor.iter().chain(&revision.combines) {
                self.retained(RETAINED_REVISIONS, (*id).into())?;
                let parent = self.reader.revision(*id).await?;
                if parent.change_id != revision.change_id {
                    return Err(invalid("predecessor belongs to another change"));
                }
                parent_generations.push(parent.generation);
            }
            if wheat_core::next_generation(parent_generations) != Some(revision.generation) {
                return Err(invalid("revision generation does not follow its parents"));
            }
            self.tree(revision.resulting_tree, aggregate).await?;
            let transition = &revision.transition;
            if !transition.preserved.is_disjoint(&transition.added)
                || transition
                    .preserved
                    .union(&transition.added)
                    .copied()
                    .collect::<BTreeSet<_>>()
                    != revision.edits
            {
                return Err(invalid("transition does not partition revision edits"));
            }
            let predecessor_edits = if let Some(predecessor) = revision.predecessor {
                self.reader.revision(predecessor).await?.edits
            } else {
                BTreeSet::new()
            };
            if !transition.preserved.is_subset(&predecessor_edits)
                || !transition.removed.is_subset(&predecessor_edits)
                || !transition.preserved.is_disjoint(&transition.removed)
            {
                return Err(invalid("transition predecessor partition mismatch"));
            }
            for subsumption in &transition.subsumed {
                if !predecessor_edits.contains(&subsumption.source)
                    || subsumption.replacements.is_empty()
                    || !subsumption.replacements.is_subset(&revision.edits)
                {
                    return Err(invalid("invalid edit subsumption"));
                }
            }
            for resolution in &transition.resolutions {
                self.retained(RETAINED_APPLICATIONS, resolution.application.into())?;
                if resolution.conflicted_edits.is_empty()
                    || !resolution.replacement_edits.is_subset(&revision.edits)
                {
                    return Err(invalid("invalid conflict resolution reference"));
                }
                let application = self.reader.application(resolution.application).await?;
                let conflict = match application.result {
                    ApplicationResultRecord::Conflicted { conflict, .. } => {
                        self.reader.conflict(conflict).await?
                    }
                    ApplicationResultRecord::Clean { .. } => {
                        return Err(invalid("resolution references clean application"));
                    }
                };
                if resolution.conflicted_edits != conflict.edits.keys().copied().collect() {
                    return Err(invalid("resolution conflict evidence mismatch"));
                }
            }
            for edit in &revision.edits {
                let definition = self.reader.edit(*edit).await?;
                match definition.operation {
                    EditOperation::GuardedText {
                        before,
                        expected,
                        replacement,
                        after,
                        ..
                    } => {
                        for id in [before, expected, replacement, after] {
                            self.required(id)?;
                        }
                    }
                    EditOperation::ExactFile {
                        expected,
                        replacement,
                    } => {
                        for id in expected
                            .into_iter()
                            .chain(replacement)
                            .map(|value| value.content)
                        {
                            self.required(id)?;
                        }
                    }
                }
            }
            let authored = self.reader.view(revision.authored_against).await?;
            let replay = self
                .replay(authored.tree, revision.resulting_tree, &revision.edits)
                .await?;
            // A combined revision follows a reapplied revision's rule: an edit already
            // satisfied where it landed is fine. Any other revision's edits all applied.
            let combined = !revision.combines.is_empty();
            if replay.result.outcomes.values().any(|outcome| {
                !matches!(
                    outcome,
                    wheat_core::edit_application::ApplicationOutcome::Applied { .. }
                ) && !(combined
                    && matches!(
                        outcome,
                        wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { .. }
                    ))
            }) {
                return Err(invalid("authored revision does not apply cleanly"));
            }
        }
        (self.observer)(RepositoryValidationPhase::VerifyApplications);
        let mut cursor = retained.cursor(state.retained_applications).await?;
        while let Some(id) = cursor.next().await? {
            let app = self
                .reader
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            self.retained(RETAINED_REVISIONS, app.revision_id.into())?;
            self.retained(RETAINED_VIEWS, app.target_view.into())?;
            let revision = self.reader.revision(app.revision_id).await?;
            let target = self.reader.view(app.target_view).await?;
            let (result_root, claimed_conflict) = match app.result {
                ApplicationResultRecord::Clean { tree } => {
                    self.tree(tree, aggregate).await?;
                    (tree, None)
                }
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    self.tree(partial_tree, aggregate).await?;
                    let conflict_record = self.reader.conflict(conflict).await?;
                    if conflict_record.base_view != app.target_view {
                        return Err(invalid("application conflict target mismatch"));
                    }
                    self.verify_conflict(conflict, &state).await?;
                    (partial_tree, Some(conflict_record))
                }
            };
            let replay = self
                .replay(target.tree, result_root, &revision.edits)
                .await?;
            let mut replay_applied = BTreeMap::new();
            let mut replay_failed = BTreeMap::new();
            for (edit, outcome) in replay.result.outcomes {
                let target_value = replay.target.get(&replay.definitions[&edit].path).copied();
                match outcome {
                    wheat_core::edit_application::ApplicationOutcome::Applied { read, write } => {
                        replay_applied.insert(
                            edit,
                            AppliedEditEvidence {
                                target: target_value,
                                read: read.map(|r| ByteRange {
                                    start: r.start as u64,
                                    end: r.end as u64,
                                }),
                                write: write.map(|r| ByteRange {
                                    start: r.start as u64,
                                    end: r.end as u64,
                                }),
                            },
                        );
                    }
                    wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { read } => {
                        replay_applied.insert(
                            edit,
                            AppliedEditEvidence {
                                target: target_value,
                                read: read.map(|r| ByteRange {
                                    start: r.start as u64,
                                    end: r.end as u64,
                                }),
                                write: None,
                            },
                        );
                    }
                    outcome => {
                        let reason = match outcome {
                            wheat_core::edit_application::ApplicationOutcome::NoMatch => EditFailure::NoMatch,
                            wheat_core::edit_application::ApplicationOutcome::Ambiguous => EditFailure::Ambiguous,
                            wheat_core::edit_application::ApplicationOutcome::Interference { with } => EditFailure::Interference { with: with.into_iter().collect() },
                            wheat_core::edit_application::ApplicationOutcome::KindMismatch { .. } => EditFailure::KindMismatch,
                            wheat_core::edit_application::ApplicationOutcome::ExpectedFileMismatch { .. } => EditFailure::ExpectedFileMismatch,
                            wheat_core::edit_application::ApplicationOutcome::Applied { .. }
                            | wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { .. } => unreachable!(),
                        };
                        replay_failed.insert(
                            edit,
                            FailedEditEvidence {
                                target: target_value,
                                reason,
                            },
                        );
                    }
                }
            }
            if replay_applied != app.applied {
                return Err(invalid("application evidence differs from semantic replay"));
            }
            match claimed_conflict {
                None if !replay_failed.is_empty() => {
                    return Err(invalid("clean application has failed edits"));
                }
                Some(conflict) if conflict.edits != replay_failed => {
                    return Err(invalid("conflict evidence differs from semantic replay"));
                }
                Some(_) if replay_failed.is_empty() => {
                    return Err(invalid("conflicted application replayed cleanly"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod untouched_tests {
    use super::*;
    use crate::LooseObjectStore;

    fn file(byte: u8) -> FileValue {
        FileValue {
            content: ContentId::from_digest([byte; 32]),
            kind: FileKind::Regular,
        }
    }

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

    async fn verify(
        objects: &LooseObjectStore,
        target: TreeId,
        result: TreeId,
        touched: &[&str],
    ) -> Result<()> {
        let directory = tempfile::tempdir().unwrap();
        let content = crate::ContentStore::new(directory.path());
        let reader = RepositoryReader::new(objects, &content);
        let verifier = Verifier {
            reader: &reader,
            scratch: VerificationScratch::new().unwrap(),
            observer: |_| {},
        };
        verifier
            .verify_untouched(
                target,
                result,
                &touched.iter().map(|path| (*path).to_owned()).collect(),
            )
            .await
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

    #[tokio::test]
    async fn untouched_allows_empty_nested_creation_and_removal_but_not_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let trees = MerkleTreeStore::new(&objects);
        let empty = node(&trees, []).await;
        let nested = node(&trees, [("leaf", DirectoryEntry::File(file(1)))]).await;
        let created = node(&trees, [("parent", DirectoryEntry::Directory(nested))]).await;
        verify(&objects, empty, created, &["parent/leaf"])
            .await
            .unwrap();
        verify(&objects, created, empty, &["parent/leaf"])
            .await
            .unwrap();
        let fabricated = node(
            &trees,
            [
                ("leaf", DirectoryEntry::File(file(1))),
                ("extra", DirectoryEntry::File(file(2))),
            ],
        )
        .await;
        let fabricated = node(&trees, [("parent", DirectoryEntry::Directory(fabricated))]).await;
        assert!(
            verify(&objects, empty, fabricated, &["parent/leaf"])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn untouched_requires_and_honors_explicit_file_directory_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let trees = MerkleTreeStore::new(&objects);
        let target = node(&trees, [("shape", DirectoryEntry::File(file(1)))]).await;
        let child = node(&trees, [("nested", DirectoryEntry::File(file(2)))]).await;
        let directory = node(&trees, [("shape", DirectoryEntry::Directory(child))]).await;
        assert!(
            verify(&objects, target, directory, &["shape/nested"])
                .await
                .is_err()
        );
        verify(&objects, target, directory, &["shape", "shape/nested"])
            .await
            .unwrap();
        let extra = node(
            &trees,
            [
                ("nested", DirectoryEntry::File(file(2))),
                ("extra", DirectoryEntry::File(file(3))),
            ],
        )
        .await;
        let extra = node(&trees, [("shape", DirectoryEntry::Directory(extra))]).await;
        assert!(
            verify(&objects, target, extra, &["shape", "shape/nested"])
                .await
                .is_err()
        );
        assert!(
            verify(&objects, directory, target, &["shape/nested"])
                .await
                .is_err()
        );
        verify(&objects, directory, target, &["shape", "shape/nested"])
            .await
            .unwrap();
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod content_map_tests {
    use super::*;
    use crate::LooseObjectStore;

    fn blob(byte: u8) -> ContentId {
        ContentId::from_digest([byte; 32])
    }

    async fn tree(objects: &LooseObjectStore, files: &[(&str, u8)]) -> TreeId {
        MerkleTreeStore::new(objects)
            .write_node(&DirectoryNode {
                entries: files
                    .iter()
                    .map(|(name, byte)| {
                        let file = FileValue {
                            content: blob(*byte),
                            kind: FileKind::Regular,
                        };
                        ((*name).to_owned(), DirectoryEntry::File(file))
                    })
                    .collect(),
            })
            .await
            .unwrap()
    }

    async fn map(objects: &LooseObjectStore, counts: &[(u8, i64)]) -> ContentMapId {
        let maps = ContentMapStore::new(objects);
        maps.adjust(
            maps.empty_root().await.unwrap(),
            counts.iter().map(|(byte, count)| (blob(*byte), *count)),
        )
        .await
        .unwrap()
    }

    fn view(tree: TreeId, required_content: ContentMapId) -> ViewRecord {
        ViewRecord {
            tree,
            revisions: RevisionMapId::from_digest([0; 32]),
            applications: ApplicationMapId::from_digest([0; 32]),
            unresolved_conflicts: ConflictSetId::from_digest([0; 32]),
            required_content,
            order: ViewOrder::BASE,
        }
    }

    /// Runs `check` with a verifier whose aggregate holds blobs 1 to 9.
    async fn with_verifier<T>(
        objects: &LooseObjectStore,
        check: impl AsyncFnOnce(&Verifier<'_, '_, LooseObjectStore, fn(RepositoryValidationPhase)>) -> T,
    ) -> T {
        let directory = tempfile::tempdir().unwrap();
        let content = crate::ContentStore::new(directory.path());
        let reader = RepositoryReader::new(objects, &content);
        let verifier = Verifier {
            reader: &reader,
            scratch: VerificationScratch::new().unwrap(),
            observer: (|_| {}) as fn(RepositoryValidationPhase),
        };
        for byte in 1..=9 {
            verifier.scratch.insert_content(blob(byte)).unwrap();
        }
        check(&verifier).await
    }

    /// The full comparison, the fallback for an edge too large to diff: every count
    /// must match, and the map may name nothing the tree does not.
    #[tokio::test]
    async fn a_map_must_count_exactly_the_tree_entries() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let files = tree(&objects, &[("a", 1), ("b", 1), ("c", 2)]).await;
        let exact = map(&objects, &[(1, 2), (2, 1)]).await;
        let undercounted = map(&objects, &[(1, 1), (2, 1)]).await;
        let extra = map(&objects, &[(1, 2), (2, 1), (3, 1)]).await;
        let missing = map(&objects, &[(1, 2)]).await;
        with_verifier(&objects, async |verifier| {
            verifier
                .content_map_matches_tree(files, exact)
                .await
                .unwrap();
            for forged in [undercounted, extra, missing] {
                assert!(
                    verifier
                        .content_map_matches_tree(files, forged)
                        .await
                        .is_err()
                );
            }
            // Earlier counts do not leak into the next comparison.
            verifier
                .content_map_matches_tree(files, exact)
                .await
                .unwrap();
        })
        .await;
    }

    /// The incremental check: a child map must differ from its parent's by exactly
    /// the net count changes of the tree diff.
    #[tokio::test]
    async fn a_child_map_must_follow_the_tree_diff() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let parent = view(
            tree(&objects, &[("a", 1), ("b", 1)]).await,
            map(&objects, &[(1, 2)]).await,
        );
        // `b` now names blob 2, and new file `c` names blob 1.
        let child_tree = tree(&objects, &[("a", 1), ("b", 2), ("c", 1)]).await;
        let exact = map(&objects, &[(1, 2), (2, 1)]).await;
        let forged = [
            map(&objects, &[(1, 1), (2, 1)]).await,
            map(&objects, &[(1, 2), (2, 2)]).await,
            map(&objects, &[(1, 2), (2, 1), (3, 1)]).await,
            map(&objects, &[(1, 2)]).await,
        ];
        with_verifier(&objects, async |verifier| {
            assert!(
                verifier
                    .content_map_follows_tree_diff(&parent, &view(child_tree, exact))
                    .await
                    .unwrap()
            );
            for forged in forged {
                assert!(
                    verifier
                        .content_map_follows_tree_diff(&parent, &view(child_tree, forged))
                        .await
                        .is_err()
                );
            }
        })
        .await;
    }

    /// A blob a child gains must be in the aggregate, even when its map is right.
    #[tokio::test]
    async fn a_gained_blob_must_be_in_the_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let parent = view(
            tree(&objects, &[("a", 1)]).await,
            map(&objects, &[(1, 1)]).await,
        );
        let child = view(
            tree(&objects, &[("a", 1), ("b", 42)]).await,
            map(&objects, &[(1, 1), (42, 1)]).await,
        );
        with_verifier(&objects, async |verifier| {
            assert!(
                verifier
                    .content_map_follows_tree_diff(&parent, &child)
                    .await
                    .is_err()
            );
        })
        .await;
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
        let cached = CachedObjects::new(self.objects, 262_144, 128 * 1024 * 1024);
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
