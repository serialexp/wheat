//! Retention cannot be released by ordinary publication; reclamation is a separate policy.
use crate::{
    ContentSetStore, ImmutableObjectStore, RepositoryPublicationError, RepositoryReader,
    RetainedSetStore,
};
use wheat_core::{RepositoryStateId, RetainedSetId};
type Result<T> = std::result::Result<T, RepositoryPublicationError>;
impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    async fn retained_subset(&self, old: RetainedSetId, new: RetainedSetId) -> Result<()> {
        let sets = RetainedSetStore::new(self.objects);
        let mut cursor = sets.cursor(old).await?;
        while let Some(id) = cursor.next().await? {
            if !sets.contains(new, id).await? {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "publication drops retained history",
                ));
            }
        }
        Ok(())
    }
    pub async fn validate_transition(
        &self,
        old: RepositoryStateId,
        new: RepositoryStateId,
    ) -> Result<()> {
        let old = self.state(old).await?;
        let new = self.state(new).await?;
        if old.base_view != new.base_view {
            return Err(RepositoryPublicationError::InvalidReferences(
                "publication changes repository base view",
            ));
        }
        for (old, new) in [
            (old.retained_changes, new.retained_changes),
            (old.retained_views, new.retained_views),
            (old.retained_revisions, new.retained_revisions),
            (old.retained_applications, new.retained_applications),
        ] {
            self.retained_subset(old, new).await?;
        }
        crate::ViewLineageMapStore::new(self.objects)
            .validate_extension(
                old.view_lineage,
                new.view_lineage,
                crate::ViewLineageLimits::default(),
            )
            .await
            .map_err(|_| {
                RepositoryPublicationError::InvalidReferences("publication drops view lineage")
            })?;
        let old = self.manifest(old.required_content).await?.content;
        let new = self.manifest(new.required_content).await?.content;
        let sets = ContentSetStore::new(self.objects);
        let mut cursor = sets.cursor(old).await?;
        while let Some(id) = cursor.next().await? {
            if !sets.contains(new, id).await? {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "publication drops retained content",
                ));
            }
        }
        Ok(())
    }
}
