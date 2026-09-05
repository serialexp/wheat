//! Inventory-driven verification avoids recursively expanding historical target views.
use crate::{
    ApplicationMapStore, ConflictSetStore, ContentSetStore, ImmutableObjectStore, MerkleTreeStore,
    RepositoryPublicationError, RepositoryReader, RetainedSetStore, RevisionMapStore,
};
use wheat_core::*;
type Result<T> = std::result::Result<T, RepositoryPublicationError>;

fn invalid(message: &'static str) -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(message)
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    async fn retained(&self, root: RetainedSetId, id: ObjectId) -> Result<()> {
        if !RetainedSetStore::new(self.objects)
            .contains(root, id)
            .await?
        {
            return Err(invalid("reference not retained"));
        }
        Ok(())
    }
    async fn retained_change(&self, root: RetainedSetId, change: ChangeId) -> Result<()> {
        // Change objects are addressed by content, not logical ChangeId. Until a
        // persistent change index exists, scan this inventory without hydrating it.
        let mut cursor = RetainedSetStore::new(self.objects).cursor(root).await?;
        while let Some(id) = cursor.next().await? {
            if self
                .change(ChangeObjectId::from_digest(*id.as_bytes()))
                .await?
                .id
                == change
            {
                return Ok(());
            }
        }
        Err(invalid("logical change not retained"))
    }

    async fn required(&self, aggregate: ContentSetId, id: ContentId) -> Result<()> {
        if !ContentSetStore::new(self.objects)
            .contains(aggregate, id)
            .await?
        {
            return Err(invalid("content absent from aggregate manifest"));
        }
        if !self.content.contains(id) {
            return Err(RepositoryPublicationError::MissingContent(id));
        }
        self.content.get(id)?;
        Ok(())
    }
    async fn tree(
        &self,
        root: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<()> {
        // Each frame owns one decoded directory; wide directory sharding is not implemented.
        let trees = MerkleTreeStore::new(self.objects);
        let mut stack = vec![trees.read_node(root).await?.entries.into_values()];
        while let Some(frame) = stack.last_mut() {
            match frame.next() {
                None => {
                    stack.pop();
                }
                Some(DirectoryEntry::File(id)) => {
                    self.required(aggregate, id).await?;
                    if let Some(visible) = visible
                        && !ContentSetStore::new(self.objects)
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
                    stack.push(trees.read_node(id).await?.entries.into_values());
                }
            }
        }
        Ok(())
    }
    async fn verify_view(
        &self,
        id: ViewId,
        state: &RepositoryStateRecord,
        aggregate: ContentSetId,
    ) -> Result<()> {
        let view = self.view(id).await?;
        let visible = self.manifest(view.required_content).await?.content;
        let mut contents = ContentSetStore::new(self.objects).cursor(visible).await?;
        while let Some(id) = contents.next().await? {
            self.required(aggregate, id).await?;
        }
        self.tree(view.tree, aggregate, Some(visible)).await?;
        let revisions = RevisionMapStore::new(self.objects);
        let applications = ApplicationMapStore::new(self.objects);
        let mut cursor = revisions.cursor(view.revisions).await?;
        while let Some((change, id)) = cursor.next().await? {
            self.retained(state.retained_revisions, id.into()).await?;
            if self.revision(id).await?.change_id != change {
                return Err(invalid("selected revision change mismatch"));
            }
        }
        let mut cursor = applications.cursor(view.applications).await?;
        while let Some((change, id)) = cursor.next().await? {
            self.retained(state.retained_applications, id.into())
                .await?;
            let app = self.application(id).await?;
            if revisions.get(view.revisions, change).await? != Some(app.revision_id) {
                return Err(invalid("selected application revision mismatch"));
            }
            if let ApplicationResultRecord::Conflicted { conflict, .. } = app.result
                && !ConflictSetStore::new(self.objects)
                    .contains(view.unresolved_conflicts, conflict)
                    .await?
            {
                return Err(invalid("selected conflict missing from view"));
            }
        }
        let mut conflicts = ConflictSetStore::new(self.objects)
            .cursor(view.unresolved_conflicts)
            .await?;
        while let Some(id) = conflicts.next().await? {
            self.verify_conflict(id, state, aggregate).await?;
            let mut selected = applications.cursor(view.applications).await?;
            let mut produced = false;
            while let Some((_, application)) = selected.next().await? {
                if matches!(self.application(application).await?.result, ApplicationResultRecord::Conflicted { conflict, .. } if conflict == id)
                {
                    produced = true;
                    break;
                }
            }
            if !produced {
                return Err(invalid(
                    "view conflict not produced by selected applications",
                ));
            }
        }
        Ok(())
    }
    async fn verify_conflict(
        &self,
        id: ConflictId,
        state: &RepositoryStateRecord,
        aggregate: ContentSetId,
    ) -> Result<()> {
        let conflict = self.conflict(id).await?;
        self.retained(state.retained_views, conflict.base_view.into())
            .await?;
        for path in conflict.paths {
            for id in [path.expected, path.actual, path.proposed]
                .into_iter()
                .flatten()
            {
                self.required(aggregate, id).await?;
            }
        }
        Ok(())
    }
    /// Verify all retained inventories using lazy collection cursors and verified reads.
    /// Work may repeat for shared trees; this does not materialize a global closure.
    pub async fn validate_complete(&self, id: RepositoryStateId) -> Result<()> {
        let state = self.state(id).await?;
        let aggregate = self.manifest(state.required_content).await?.content;
        let mut contents = ContentSetStore::new(self.objects).cursor(aggregate).await?;
        while let Some(id) = contents.next().await? {
            self.required(aggregate, id).await?;
        }
        for id in [state.base_view, state.working_view, state.published_view] {
            self.retained(state.retained_views, id.into()).await?;
        }
        let retained = RetainedSetStore::new(self.objects);
        let mut cursor = retained.cursor(state.retained_changes).await?;
        while let Some(id) = cursor.next().await? {
            self.change(ChangeObjectId::from_digest(*id.as_bytes()))
                .await?;
        }
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            self.verify_view(ViewId::from_digest(*id.as_bytes()), &state, aggregate)
                .await?;
        }
        let mut cursor = retained.cursor(state.retained_revisions).await?;
        while let Some(id) = cursor.next().await? {
            let revision = self
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            self.retained(state.retained_views, revision.authored_against.into())
                .await?;
            self.retained_change(state.retained_changes, revision.change_id)
                .await?;
            for dependency in &revision.dependencies {
                self.retained_change(state.retained_changes, *dependency)
                    .await?;
            }
            if let Some(id) = revision.predecessor {
                self.retained(state.retained_revisions, id.into()).await?;
                if self.revision(id).await?.change_id != revision.change_id {
                    return Err(invalid("predecessor belongs to another change"));
                }
            }
            self.tree(revision.resulting_tree, aggregate, None).await?;
            for edit in revision.transformation.edits {
                for id in [edit.expected, edit.replacement].into_iter().flatten() {
                    self.required(aggregate, id).await?;
                }
            }
        }
        let mut cursor = retained.cursor(state.retained_applications).await?;
        while let Some(id) = cursor.next().await? {
            let app = self
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            self.retained(state.retained_revisions, app.revision_id.into())
                .await?;
            self.retained(state.retained_views, app.target_view.into())
                .await?;
            match app.result {
                ApplicationResultRecord::Clean { tree } => self.tree(tree, aggregate, None).await?,
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    self.tree(partial_tree, aggregate, None).await?;
                    if self.conflict(conflict).await?.base_view != app.target_view {
                        return Err(invalid("application conflict target mismatch"));
                    }
                    self.verify_conflict(conflict, &state, aggregate).await?;
                }
            }
        }
        Ok(())
    }
}
