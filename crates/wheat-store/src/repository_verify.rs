//! Inventory-driven verification with a per-run disk-backed membership and memo index.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Mutex,
};

use async_trait::async_trait;

use crate::{
    ApplicationMapStore, ConflictSetStore, ContentSetStore, ImmutableObjectStore, MerkleTreeStore,
    ObjectStoreError, RepositoryPublicationError, RepositoryReader, RetainedSetStore,
    RevisionMapStore, StoredObject, verification_scratch::VerificationScratch,
};
use wheat_core::*;
type Result<T> = std::result::Result<T, RepositoryPublicationError>;
const RETAINED_CHANGES: u8 = 1;
const RETAINED_VIEWS: u8 = 2;
const RETAINED_REVISIONS: u8 = 3;
const RETAINED_APPLICATIONS: u8 = 4;

fn invalid(message: &'static str) -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(message)
}

struct CachedObjects<'a, O> {
    inner: &'a O,
    max_entries: usize,
    max_bytes: usize,
    cache: Mutex<(HashMap<ObjectId, StoredObject>, VecDeque<ObjectId>, usize)>,
}

impl<'a, O> CachedObjects<'a, O> {
    fn new(inner: &'a O, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner,
            max_entries,
            max_bytes,
            cache: Mutex::new((HashMap::new(), VecDeque::new(), 0)),
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
        while (cache.0.len() == self.max_entries || cache.2 + object_bytes > self.max_bytes)
            && let Some(oldest) = cache.1.pop_front()
        {
            if let Some(evicted) = cache.0.remove(&oldest) {
                cache.2 -= evicted.canonical_bytes.len();
            }
        }
        if object_bytes <= self.max_bytes {
            cache.1.push_back(id);
            cache.2 += object_bytes;
            cache.0.insert(id, object.clone());
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

struct Verifier<'a, 'r, O> {
    reader: &'r RepositoryReader<'a, O>,
    scratch: VerificationScratch,
}

impl<O: ImmutableObjectStore> Verifier<'_, '_, O> {
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

    async fn tree(
        &self,
        root: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<()> {
        if self.scratch.tree_verified(root, aggregate, visible)? {
            return Ok(());
        }
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut stack = vec![(root, trees.read_node(root).await?.entries.into_values())];
        while let Some((node, frame)) = stack.last_mut() {
            match frame.next() {
                None => {
                    // Only memoize a subtree after every descendant passed in
                    // this exact aggregate/visible-manifest context.
                    self.scratch.record_tree(*node, aggregate, visible)?;
                    stack.pop();
                }
                Some(DirectoryEntry::File(id)) => {
                    self.required(id)?;
                    if let Some(visible) = visible
                        && !ContentSetStore::new(self.reader.objects)
                            .contains(visible, id)
                            .await?
                    {
                        return Err(invalid("visible file absent from view manifest"));
                    }
                }
                Some(DirectoryEntry::Directory(id)) => {
                    if stack.len() >= 1024 {
                        return Err(invalid("directory depth exceeds verification limit"));
                    }
                    if !self.scratch.tree_verified(id, aggregate, visible)? {
                        stack.push((id, trees.read_node(id).await?.entries.into_values()));
                    }
                }
            }
        }
        // Failed traversals never produce receipts.
        self.scratch.record_tree(root, aggregate, visible)?;
        Ok(())
    }

    async fn visible_set(&self, root: ContentSetId, aggregate: ContentSetId) -> Result<()> {
        let store = ContentSetStore::new(self.reader.objects);
        if self.scratch.visible_set_verified(root, aggregate)? {
            return Ok(());
        }
        let node = store.read_node(root).await?;
        if node.depth != 0 {
            return Err(invalid("visible content root depth is not zero"));
        }
        let mut key = [0; 32];
        let mut stack = vec![(root, node, 0usize, key)];
        while let Some((id, node, next, node_key)) = stack.last_mut() {
            if *next == 0 && node.present {
                if node.depth != 32 {
                    return Err(invalid("content-set value before terminal depth"));
                }
                self.required(ContentId::from_digest(*node_key))?;
            }
            if *next == node.children.len() {
                self.scratch.record_visible_set(*id, aggregate)?;
                stack.pop();
                continue;
            }
            let child = node.children[*next];
            *next += 1;
            if self.scratch.visible_set_verified(child.child, aggregate)? {
                continue;
            }
            let child_node = store.read_node(child.child).await?;
            if child_node.depth != node.depth + 1 || usize::from(node.depth) >= key.len() {
                return Err(invalid("invalid content-set child depth"));
            }
            key = *node_key;
            key[usize::from(node.depth)] = child.edge;
            stack.push((child.child, child_node, 0, key));
        }
        Ok(())
    }

    async fn revision_map(&self, root: RevisionMapId, retained: RetainedSetId) -> Result<()> {
        let store = RevisionMapStore::new(self.reader.objects);
        if self.scratch.revision_map_verified(root, retained)? {
            return Ok(());
        }
        let node = store.read_node(root).await?;
        if node.depth != 0 {
            return Err(invalid("revision-map root depth is not zero"));
        }
        let mut key = [0; 16];
        let mut stack = vec![(root, node, 0usize, key)];
        while let Some((id, node, next, node_key)) = stack.last_mut() {
            if *next == 0
                && let Some(revision_id) = node.value
            {
                if node.depth != 16 {
                    return Err(invalid("revision-map value before terminal depth"));
                }
                self.retained(RETAINED_REVISIONS, revision_id.into())?;
                if self.reader.revision(revision_id).await?.change_id
                    != ChangeId::from_bytes(*node_key)
                {
                    return Err(invalid("selected revision change mismatch"));
                }
            }
            if *next == node.children.len() {
                self.scratch.record_revision_map(*id, retained)?;
                stack.pop();
                continue;
            }
            let child = node.children[*next];
            *next += 1;
            if self.scratch.revision_map_verified(child.child, retained)? {
                continue;
            }
            let child_node = store.read_node(child.child).await?;
            if child_node.depth != node.depth + 1 || usize::from(node.depth) >= key.len() {
                return Err(invalid("invalid revision-map child depth"));
            }
            key = *node_key;
            key[usize::from(node.depth)] = child.edge;
            stack.push((child.child, child_node, 0, key));
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
        let app_node = applications.read_node(application_root).await?;
        let revision_node = revisions.read_node(revision_root).await?;
        if app_node.depth != 0 || revision_node.depth != 0 {
            return Err(invalid("selection map root depth is not zero"));
        }
        let mut produced = HashSet::new();
        let mut key = [0; 16];
        // The final bool marks whether this subtree contains a conflict. Receipts are only
        // emitted for clean subtrees, so a pruned receipt cannot hide produced conflicts.
        let mut stack = vec![(
            application_root,
            revision_root,
            app_node,
            revision_node,
            0usize,
            key,
            false,
        )];
        while let Some((app_id, revision_id, app, revision, next, node_key, conflicted)) =
            stack.last_mut()
        {
            if *next == 0
                && let Some(application_id) = app.value
            {
                if app.depth != 16 || revision.depth != 16 {
                    return Err(invalid("selection map value before terminal depth"));
                }
                self.retained(RETAINED_APPLICATIONS, application_id.into())?;
                let record = self.reader.application(application_id).await?;
                if revision.value != Some(record.revision_id) {
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
                    *conflicted = true;
                }
                let _change = ChangeId::from_bytes(*node_key);
            }
            if *next == app.children.len() {
                let completed = (*app_id, *revision_id, *conflicted);
                if !completed.2 {
                    self.scratch.record_map_pair(
                        completed.0,
                        completed.1,
                        retained,
                        conflict_root,
                    )?;
                }
                stack.pop();
                if completed.2
                    && let Some(parent) = stack.last_mut()
                {
                    parent.6 = true;
                }
                continue;
            }
            let app_child = app.children[*next];
            *next += 1;
            let Some(revision_child) = revision
                .children
                .iter()
                .find(|child| child.edge == app_child.edge)
                .copied()
            else {
                return Err(invalid("application selection absent from revision map"));
            };
            if self.scratch.map_pair_verified(
                app_child.child,
                revision_child.child,
                retained,
                conflict_root,
            )? {
                continue;
            }
            let child_app = applications.read_node(app_child.child).await?;
            let child_revision = revisions.read_node(revision_child.child).await?;
            if child_app.depth != app.depth + 1
                || child_revision.depth != revision.depth + 1
                || child_app.depth != child_revision.depth
                || usize::from(app.depth) >= key.len()
            {
                return Err(invalid("invalid selection map child depth"));
            }
            key = *node_key;
            key[usize::from(app.depth)] = app_child.edge;
            stack.push((
                app_child.child,
                revision_child.child,
                child_app,
                child_revision,
                0,
                key,
                false,
            ));
        }
        Ok(produced)
    }

    async fn verify_view(
        &self,
        id: ViewId,
        state: &RepositoryStateRecord,
        aggregate: ContentSetId,
    ) -> Result<()> {
        let view = self.reader.view(id).await?;
        let visible = self.reader.manifest(view.required_content).await?.content;
        self.visible_set(visible, aggregate).await?;
        self.tree(view.tree, aggregate, Some(visible)).await?;
        self.revision_map(view.revisions, state.retained_revisions)
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
            self.verify_conflict(id, state).await?;
            if !produced.contains(&id) {
                return Err(invalid(
                    "view conflict not produced by selected applications",
                ));
            }
        }
        Ok(())
    }

    async fn verify_conflict(&self, id: ConflictId, _state: &RepositoryStateRecord) -> Result<()> {
        let conflict = self.reader.conflict(id).await?;
        self.retained(RETAINED_VIEWS, conflict.base_view.into())?;
        for path in conflict.paths {
            for id in [path.expected, path.actual, path.proposed]
                .into_iter()
                .flatten()
            {
                self.required(id)?;
            }
        }
        Ok(())
    }

    async fn validate(&self, id: RepositoryStateId) -> Result<()> {
        let state = self.reader.state(id).await?;
        let aggregate = self.reader.manifest(state.required_content).await?.content;

        // Authenticate each retained inventory once and use exact disk-backed
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
        let mut contents = ContentSetStore::new(self.reader.objects)
            .cursor(aggregate)
            .await?;
        while let Some(id) = contents.next().await? {
            self.scratch.insert_content(id)?;
            if !self.reader.content.contains(id) {
                return Err(RepositoryPublicationError::MissingContent(id));
            }
            self.reader.content.get(id)?;
        }

        // Change objects are addressed by content. Build the logical index exactly once,
        // before checking references, and reject every duplicate logical identity.
        let retained = RetainedSetStore::new(self.reader.objects);
        let mut cursor = retained.cursor(state.retained_changes).await?;
        while let Some(id) = cursor.next().await? {
            let object = ChangeObjectId::from_digest(*id.as_bytes());
            let change = self.reader.change(object).await?;
            if !self.scratch.insert_change(change.id, object)? {
                return Err(invalid("duplicate logical change id"));
            }
        }

        for id in [state.base_view, state.working_view, state.published_view] {
            self.retained(RETAINED_VIEWS, id.into())?;
        }
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            self.verify_view(ViewId::from_digest(*id.as_bytes()), &state, aggregate)
                .await?;
        }
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
            for edit in revision.transformation.edits {
                for id in [edit.expected, edit.replacement].into_iter().flatten() {
                    self.required(id)?;
                }
            }
        }
        let mut cursor = retained.cursor(state.retained_applications).await?;
        while let Some(id) = cursor.next().await? {
            let app = self
                .reader
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            self.retained(RETAINED_REVISIONS, app.revision_id.into())?;
            self.retained(RETAINED_VIEWS, app.target_view.into())?;
            match app.result {
                ApplicationResultRecord::Clean { tree } => self.tree(tree, aggregate, None).await?,
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    self.tree(partial_tree, aggregate, None).await?;
                    if self.reader.conflict(conflict).await?.base_view != app.target_view {
                        return Err(invalid("application conflict target mismatch"));
                    }
                    self.verify_conflict(conflict, &state).await?;
                }
            }
        }
        Ok(())
    }
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// Verify all retained inventories with verified reads and fresh, disk-backed scratch indexes.
    pub async fn validate_complete(&self, id: RepositoryStateId) -> Result<()> {
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
        }
        .validate(id)
        .await
    }
}
