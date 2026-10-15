//! Explicit reference-model hydration. Unlike lazy reads, this intentionally
//! loads retained history into memory and is not the streaming-import path.
use crate::{
    ApplicationMapStore, ConflictSetStore, ContentMapStore, ImmutableObjectStore, MerkleTreeStore,
    RepositoryPublicationError, RepositoryReader, RetainedSetStore, RevisionMapStore,
};
use std::collections::{BTreeMap, BTreeSet};
use wheat_core::*;
type Result<T> = std::result::Result<T, RepositoryPublicationError>;

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    pub(crate) async fn hydrate_tree(&self, root: TreeId) -> Result<Tree> {
        let trees = MerkleTreeStore::new(self.objects);
        let mut result = Tree::new();
        let mut stack = vec![(
            String::new(),
            trees.read_node(root).await?.entries.into_iter(),
        )];
        while let Some((prefix, entries)) = stack.last_mut() {
            let Some((name, entry)) = entries.next() else {
                stack.pop();
                continue;
            };
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            match entry {
                DirectoryEntry::File(id) => {
                    result.insert(path, id);
                }
                DirectoryEntry::Directory(id) => {
                    if stack.len() >= 1024 {
                        return Err(RepositoryPublicationError::InvalidReferences(
                            "directory depth exceeds hydration limit",
                        ));
                    }
                    stack.push((path, trees.read_node(id).await?.entries.into_iter()));
                }
            }
        }
        Ok(result)
    }

    /// Reconstruct an editable reference repository from canonical storage only.
    /// This deliberately materializes all retained history; large-history callers
    /// should use the lazy record APIs instead. No YAML snapshot is consulted.
    pub async fn hydrate(&self, root: RepositoryStateId) -> Result<Repository> {
        self.validate_complete(root).await?;
        let state = self.state(root).await?;
        let retained = RetainedSetStore::new(self.objects);
        let mut changes = Vec::new();
        let mut cursor = retained.cursor(state.retained_changes).await?;
        while let Some(id) = cursor.next().await? {
            changes.push(
                self.change(ChangeObjectId::from_digest(*id.as_bytes()))
                    .await?,
            );
        }
        let mut views = Vec::new();
        let mut cursor = retained.cursor(state.retained_views).await?;
        while let Some(id) = cursor.next().await? {
            let id = ViewId::from_digest(*id.as_bytes());
            let record = self.view(id).await?;
            let tree = self.hydrate_tree(record.tree).await?;
            let mut frontier = BTreeMap::new();
            let mut selections = RevisionMapStore::new(self.objects)
                .cursor(record.revisions)
                .await?;
            while let Some((key, value)) = selections.next().await? {
                frontier.insert(key, value);
            }
            let mut applications = BTreeMap::new();
            let mut selections = ApplicationMapStore::new(self.objects)
                .cursor(record.applications)
                .await?;
            while let Some((key, value)) = selections.next().await? {
                applications.insert(key, value);
            }
            let mut unresolved_conflicts = BTreeSet::new();
            let mut conflicts = ConflictSetStore::new(self.objects)
                .cursor(record.unresolved_conflicts)
                .await?;
            while let Some(id) = conflicts.next().await? {
                unresolved_conflicts.insert(id);
            }
            let mut required_content = BTreeMap::new();
            let mut contents = ContentMapStore::new(self.objects)
                .cursor(record.required_content)
                .await?;
            while let Some((id, count)) = contents.next().await? {
                required_content.insert(id, count);
            }
            views.push(RepositoryView {
                id,
                tree,
                frontier,
                applications,
                unresolved_conflicts,
                required_content,
            });
        }
        let mut revisions = Vec::new();
        let mut cursor = retained.cursor(state.retained_revisions).await?;
        while let Some(id) = cursor.next().await? {
            let record = self
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            let tree = self.hydrate_tree(record.resulting_tree).await?;
            let mut edits = BTreeMap::new();
            for edit_id in &record.edits {
                edits.insert(*edit_id, self.edit(*edit_id).await?);
            }
            revisions.push((record, tree, edits));
        }
        let mut applications = Vec::new();
        let mut cursor = retained.cursor(state.retained_applications).await?;
        while let Some(id) = cursor.next().await? {
            let record = self
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            let (tree, conflict) = match record.result {
                ApplicationResultRecord::Clean { tree } => (self.hydrate_tree(tree).await?, None),
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => (
                    self.hydrate_tree(partial_tree).await?,
                    Some(self.conflict(conflict).await?),
                ),
            };
            applications.push((record, tree, conflict));
        }
        let lineage = crate::ViewLineageMapStore::new(self.objects);
        let mut view_lineage = BTreeSet::new();
        for parent in &views {
            let Some(children) =
                lineage
                    .get(state.view_lineage, parent.id)
                    .await
                    .map_err(|_| {
                        RepositoryPublicationError::InvalidReferences("invalid view lineage")
                    })?
            else {
                continue;
            };
            let mut children = retained.cursor(children).await?;
            while let Some(child) = children.next().await? {
                view_lineage.insert((parent.id, ViewId::from_digest(*child.as_bytes())));
            }
        }
        let repository = Repository::from_parts(RepositoryParts {
            state,
            changes,
            views,
            revisions,
            applications,
            view_lineage,
        })?;
        // A reconstruction must not silently invent or drop retained commitments.
        if repository.canonical_closure().state.id != root {
            return Err(RepositoryPublicationError::InvalidReferences(
                "hydrated repository state differs from persisted root",
            ));
        }
        Ok(repository)
    }
}
