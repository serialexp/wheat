//! Bounded transition validation using delta comparison.
//!
//! Verifies only the difference between a baseline state and a proposed state,
//! using `paired_diff` to skip equal subtrees. This is O(delta) rather than
//! O(repository), making publication latency proportional to the change size
//! instead of the total repository size.
//!
//! The baseline is assumed to have been validated when it was originally
//! published (or at import time). Nothing in the baseline is re-verified.

use std::collections::HashSet;

use wheat_core::{ObjectId, RepositoryStateId, ViewId, validate_canonical_object};

use crate::{
    ContentSetStore, ContentStoreError, ImmutableObjectStore, ObjectStoreError,
    RepositoryPublicationError, RepositoryReader, RetainedSetStore,
    repository_verify::TransitionValidationOptions,
    sets::{RetainedSetDelta, RetainedSetDeltaLimits},
};

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

fn invalid(message: &'static str) -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(message)
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// Verify only the transition delta between a baseline and a proposed state.
    ///
    /// Monotonic by default: rejects removal of retained entries unless
    /// `options.allow_non_monotonic` is set (force-publish for secret removal,
    /// repository administration). New objects are validated for canonical
    /// encoding and identity, and new content entries are checked for existence.
    pub async fn validate_transition(
        &self,
        old: RepositoryStateId,
        new: RepositoryStateId,
    ) -> Result<()> {
        self.validate_transition_with_options(old, new, TransitionValidationOptions::default())
            .await
    }

    /// Validate the transition with explicit options (e.g. force-publish).
    pub async fn validate_transition_with_options(
        &self,
        old: RepositoryStateId,
        new: RepositoryStateId,
        options: TransitionValidationOptions,
    ) -> Result<()> {
        let baseline = self.state(old).await?;
        let proposed = self.state(new).await?;

        // The base view is immutable per repository — it is the root of the
        // lineage graph and cannot change through ordinary publication.
        if !options.allow_non_monotonic && baseline.base_view != proposed.base_view {
            return Err(invalid("publication changes repository base view"));
        }

        // 1. Delta each retained set and collect newly added object IDs.
        let retained = RetainedSetStore::new(self.objects);
        let limits = RetainedSetDeltaLimits::default();
        let mut added_objects: HashSet<ObjectId> = HashSet::new();

        for (old_root, new_root) in [
            (baseline.retained_changes, proposed.retained_changes),
            (baseline.retained_views, proposed.retained_views),
            (baseline.retained_revisions, proposed.retained_revisions),
            (
                baseline.retained_applications,
                proposed.retained_applications,
            ),
        ] {
            if old_root == new_root {
                continue;
            }
            let delta = retained.delta(old_root, new_root, limits).await?;
            for entry in &delta {
                match entry {
                    RetainedSetDelta::Added(id) => {
                        added_objects.insert(*id);
                    }
                    RetainedSetDelta::Removed(_) => {
                        if !options.allow_non_monotonic {
                            return Err(invalid(
                                "publication drops retained history",
                            ));
                        }
                    }
                }
            }
        }

        // 2. Validate canonical encoding of every newly added object.
        for &id in &added_objects {
            let stored = self.objects.get(id).await.map_err(|error| match error {
                ObjectStoreError::Missing(id) => RepositoryPublicationError::MissingObject(id),
                other => RepositoryPublicationError::Object(other),
            })?;
            let actual = ObjectId::of_canonical_bytes(&stored.canonical_bytes);
            if actual != id {
                return Err(invalid("new object identity mismatch"));
            }
            validate_canonical_object(&stored.canonical_bytes)
                .map_err(|_| invalid("new object does not decode as a valid canonical object"))?;
        }

        // 3. Delta the change map — verify new change entries reference retained objects.
        if baseline.changes != proposed.changes {
            let changes = crate::ChangeMapStore::new(self.objects);
            let delta = changes
                .delta(baseline.changes, proposed.changes, Default::default())
                .await?;
            for entry in &delta {
                match entry {
                    crate::change_map::ChangeMapDelta::Added { object, .. }
                    | crate::change_map::ChangeMapDelta::Changed {
                        new_object: object, ..
                    } => {
                        let oid = ObjectId::from_digest(*object.as_bytes());
                        if !added_objects.contains(&oid)
                            && !retained
                                .contains(baseline.retained_changes, oid)
                                .await?
                        {
                            return Err(invalid(
                                "change map references object not in retained changes",
                            ));
                        }
                    }
                    crate::change_map::ChangeMapDelta::Removed { .. } => {
                        if !options.allow_non_monotonic {
                            return Err(invalid(
                                "publication drops change-map entries",
                            ));
                        }
                    }
                }
            }
        }

        // 4. Verify view lineage extension (existing bounded method).
        //    Force-publish may legitimately rewrite lineage (history removal).
        if baseline.view_lineage != proposed.view_lineage && !options.allow_non_monotonic {
            crate::ViewLineageMapStore::new(self.objects)
                .validate_extension(
                    baseline.view_lineage,
                    proposed.view_lineage,
                    crate::ViewLineageLimits::default(),
                )
                .await
                .map_err(|_| invalid("publication drops view lineage"))?;
        }

        // 5. Delta the content set — verify new content entries exist.
        let baseline_content = self.manifest(baseline.required_content).await?.content;
        let proposed_content = self.manifest(proposed.required_content).await?.content;
        if baseline_content != proposed_content {
            let content_sets = ContentSetStore::new(self.objects);
            let delta = content_sets
                .delta(baseline_content, proposed_content, Default::default())
                .await?;
            for entry in &delta {
                match entry {
                    crate::sets::ContentSetDelta::Added(content_id) => {
                        match self.content.get(*content_id) {
                            Ok(_) => {}
                            Err(ContentStoreError::Missing(_)) => {
                                return Err(RepositoryPublicationError::MissingContent(
                                    *content_id,
                                ));
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    crate::sets::ContentSetDelta::Removed(_) => {
                        if !options.allow_non_monotonic {
                            return Err(invalid("publication drops retained content"));
                        }
                    }
                }
            }
        }

        // 6. Verify newly retained views have revision maps referencing retained revisions.
        if baseline.retained_views != proposed.retained_views {
            let view_delta = retained
                .delta(baseline.retained_views, proposed.retained_views, limits)
                .await?;
            let revisions = crate::RevisionMapStore::new(self.objects);
            for entry in &view_delta {
                if let RetainedSetDelta::Added(id) = entry {
                    let view_id = ViewId::from_digest(*id.as_bytes());
                    let view = self.view(view_id).await?;

                    let mut cursor = revisions.cursor(view.revisions).await?;
                    while let Some((_change_id, revision_id)) = cursor.next().await? {
                        let oid = ObjectId::from_digest(*revision_id.as_bytes());
                        if !added_objects.contains(&oid)
                            && !retained
                                .contains(baseline.retained_revisions, oid)
                                .await?
                        {
                            return Err(invalid(
                                "view revision map references non-retained revision",
                            ));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
