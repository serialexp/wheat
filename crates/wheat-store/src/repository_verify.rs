//! Inventory-driven verification with a per-run disk-backed membership and memo index.
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    num::NonZeroUsize,
    sync::Mutex,
};

use async_trait::async_trait;
use lru::LruCache;

use crate::{
    ApplicationMapStore, ConflictSetStore, ContentSetStore, ContentStoreError,
    ImmutableObjectStore, MerkleTreeStore, ObjectStoreError, RepositoryPublicationError,
    RepositoryReader, RetainedSetStore, RevisionMapStore, StoredObject,
    radix::{ContextWalkStep, Store as RadixStore},
    revision_map::RevisionCollection,
    sets::ContentCollection,
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
    VerifyViewContentSets,
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
            Self::VerifyViewContentSets => "verify_view_content_sets",
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

    async fn tree(
        &self,
        root: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<()> {
        self.authenticate_tree(root).await?;
        if self
            .scratch
            .tree_semantics_verified(root, aggregate, visible)?
        {
            return Ok(());
        }
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![(root, trees.read_node(root).await?.entries.into_values())];
        while let Some((node, frame)) = stack.last_mut() {
            match frame.next() {
                None => {
                    // Only memoize a subtree after every descendant passed in
                    // this exact aggregate/visible-manifest context.
                    self.scratch
                        .record_tree_semantics(*node, aggregate, visible)?;
                    stack.pop();
                }
                Some(DirectoryEntry::File(value)) => {
                    self.required(value.content)?;
                    if let Some(visible) = visible
                        && !ContentSetStore::new(self.reader.objects)
                            .contains(visible, value.content)
                            .await?
                    {
                        return Err(invalid("visible file absent from view manifest"));
                    }
                }
                Some(DirectoryEntry::Directory(id)) => {
                    if !self
                        .scratch
                        .tree_semantics_verified(id, aggregate, visible)?
                    {
                        stack.push((id, trees.read_node(id).await?.entries.into_values()));
                    }
                }
            }
        }
        Ok(())
    }

    async fn visible_set(&self, root: ContentSetId, aggregate: ContentSetId) -> Result<()> {
        let store = RadixStore::<_, ContentCollection>::new(self.reader.objects);
        let mut walk = store
            .context_walk(root)
            .await
            .map_err(crate::CollectionError::from)?;
        while let Some(step) = walk.next().await.map_err(crate::CollectionError::from)? {
            match step {
                ContextWalkStep::Node { id } => {
                    if self.scratch.radix_node_verified(
                        RadixReceiptKind::VisibleContent,
                        aggregate.as_bytes(),
                        id.into(),
                        walk.prefix(),
                    )? {
                        walk.prune();
                    }
                }
                ContextWalkStep::Entry { key: content, .. } => self.required(content)?,
                ContextWalkStep::Complete { id } => self.scratch.record_radix_node(
                    RadixReceiptKind::VisibleContent,
                    aggregate.as_bytes(),
                    id.into(),
                    walk.prefix(),
                )?,
            }
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
    ) -> Result<HashSet<ConflictId>> {
        let applications = ApplicationMapStore::new(self.reader.objects);
        let revisions = RevisionMapStore::new(self.reader.objects);
        if self.scratch.map_pair_verified(
            application_root,
            revision_root,
            retained,
            conflict_root,
        )? {
            return Ok(HashSet::new());
        }
        let mut produced = HashSet::new();
        let mut cursor = applications.cursor(application_root).await?;
        while let Some((change_id, application_id)) = cursor.next().await? {
            self.retained(RETAINED_APPLICATIONS, application_id.into())?;
            let record = self.reader.application(application_id).await?;
            if revisions.get(revision_root, change_id).await? != Some(record.revision_id) {
                return Err(invalid("selected application revision mismatch"));
            }
            if let ApplicationResultRecord::Conflicted { conflict, .. } = record.result {
                if !ConflictSetStore::new(self.reader.objects)
                    .contains(conflict_root, conflict)
                    .await?
                {
                    return Err(invalid("selected conflict missing from view"));
                }
                produced.insert(conflict);
            }
        }
        if produced.is_empty() {
            self.scratch.record_map_pair(
                application_root,
                revision_root,
                retained,
                conflict_root,
            )?;
        }
        Ok(produced)
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
    ) -> Result<wheat_core::edit_application::ApplicationResult> {
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut target = Tree::new();
        let mut definitions = Vec::with_capacity(edit_ids.len());
        for id in edit_ids {
            let edit = self.reader.edit(*id).await?;
            let components = edit.path.split('/').map(str::to_owned).collect::<Vec<_>>();
            // The commit writer treats a file encountered before a requested
            // descendant as an absent leaf. A simultaneous boundary deletion
            // can then replace that file with the requested directory.
            let value = match trees.lookup(target_root, &components).await {
                Ok(value) => value,
                Err(crate::MerkleTreeError::FileInPath(_)) => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(value) = value {
                target.insert(edit.path.clone(), value);
            }
            definitions.push((*id, edit));
        }
        let touched = definitions
            .iter()
            .map(|(_, edit)| edit.path.clone())
            .collect();
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
        for (_, edit) in &definitions {
            let components = edit.path.split('/').map(str::to_owned).collect::<Vec<_>>();
            if trees.lookup(result_root, &components).await? != replay.tree.get(&edit.path).copied()
            {
                return Err(invalid("claimed result differs from semantic replay"));
            }
        }
        Ok(replay)
    }

    async fn verify_lineage_semantics(&self, state: &RepositoryStateRecord) -> Result<()> {
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
        for view in &retained_views {
            if *view != state.base_view && incoming.get(view).copied().unwrap_or_default() == 0 {
                return Err(invalid("retained non-base view lacks incoming lineage"));
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
        Ok(())
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
        let aggregate = self.reader.manifest(state.required_content).await?.content;

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
        self.verify_lineage_semantics(&state).await?;
        (self.observer)(RepositoryValidationPhase::VerifyViewContentSets);
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            let view = self
                .reader
                .view(ViewId::from_digest(*id.as_bytes()))
                .await?;
            let visible = self.reader.manifest(view.required_content).await?.content;
            self.visible_set(visible, aggregate).await?;
        }
        (self.observer)(RepositoryValidationPhase::VerifyViewTrees);
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            let view = self
                .reader
                .view(ViewId::from_digest(*id.as_bytes()))
                .await?;
            let visible = self.reader.manifest(view.required_content).await?.content;
            self.tree(view.tree, aggregate, Some(visible)).await?;
        }
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
            let produced = self
                .application_map(
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
                if !produced.contains(&id) {
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
            if let Some(id) = revision.predecessor {
                self.retained(RETAINED_REVISIONS, id.into())?;
                if self.reader.revision(id).await?.change_id != revision.change_id {
                    return Err(invalid("predecessor belongs to another change"));
                }
            }
            self.tree(revision.resulting_tree, aggregate, None).await?;
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
            if replay.outcomes.values().any(|outcome| {
                !matches!(
                    outcome,
                    wheat_core::edit_application::ApplicationOutcome::Applied { .. }
                )
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
                    self.tree(tree, aggregate, None).await?;
                    (tree, None)
                }
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    self.tree(partial_tree, aggregate, None).await?;
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
            for (edit, outcome) in replay.outcomes {
                let definition = self.reader.edit(edit).await?;
                let components = definition
                    .path
                    .split('/')
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let target_value = MerkleTreeStore::new(self.reader.objects)
                    .lookup(target.tree, &components)
                    .await?;
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
