//! Joining frontiers: one view holding the work of several views that grew apart below
//! one basis, such as a label's view after a fetch kept local work beside the remote's.
//!
//! The joined view is the basis with every change the frontiers add composed onto it in
//! canonical order: dependencies first, ties broken by the lowest change id. Changes
//! keep their revisions, so where a frontier's own composition is repeated the
//! applications and views come out the same objects. An application that conflicts is
//! recorded as a durable conflict, as in any composition. Each frontier gets a lineage
//! edge to the joined view, which leaves it the one frontier.

use std::collections::{BTreeMap, BTreeSet};

use wheat_core::{ChangeId, RevisionId, RevisionRecord, ViewId};

use crate::{
    ImmutableObjectStore, PersistedRepositoryMutator, PreparedRepositoryMutation, PublicationStore,
    RepositoryHead, RepositoryMutationError, RepositoryPublicationError, RepositoryReader,
    RetainedSetStore, RevisionMapDelta, RevisionMapStore,
    repository_mutation::{Fresh, topological_by},
};

type Result<T> = std::result::Result<T, RepositoryMutationError>;

/// What joining frontiers composed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationOutcome {
    pub resulting_view: ViewId,
    /// The changes composed onto the basis, in canonical order.
    pub composed: Vec<ChangeId>,
    /// The composed changes whose application conflicted.
    pub conflicted: Vec<ChangeId>,
}

impl<O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'_, O, P> {
    /// Joins `frontiers`, each holding every change of `basis` at the basis's revision,
    /// into one view.
    ///
    /// Refused when a frontier revises or drops a change the basis holds, or when two
    /// frontiers hold different revisions of one change. Only what the frontiers add
    /// to the basis is read, so the cost follows the work being joined, not the
    /// history below the basis.
    #[tracing::instrument(name = "integrate", level = "debug", skip_all, fields(frontiers = frontiers.len()))]
    pub async fn integrate(
        &self,
        expected: RepositoryHead,
        basis: ViewId,
        frontiers: &[ViewId],
    ) -> Result<(PreparedRepositoryMutation, IntegrationOutcome)> {
        if frontiers.is_empty() {
            return Err(RepositoryMutationError::NoFrontiers);
        }
        let actual = self
            .publications
            .read()
            .map_err(RepositoryPublicationError::from)?;
        if actual != expected {
            return Err(RepositoryMutationError::Stale { expected, actual });
        }
        let reader = RepositoryReader::new(self.objects, self.content);
        let state = reader
            .state(reader.envelope(expected.envelope).await?.state)
            .await?;
        let retained = RetainedSetStore::new(self.objects);
        for view in std::iter::once(&basis).chain(frontiers) {
            if !retained
                .contains(state.retained_views, (*view).into())
                .await?
            {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "integrated view is not retained",
                )
                .into());
            }
        }
        let base = reader.view(basis).await?;

        // What each frontier adds to the basis.
        let maps = RevisionMapStore::new(self.objects);
        let mut added: BTreeMap<ChangeId, RevisionId> = BTreeMap::new();
        let mut divergent = BTreeSet::new();
        let mut revises_basis = BTreeSet::new();
        for frontier in frontiers {
            let view = reader.view(*frontier).await?;
            for entry in maps
                .delta(base.revisions, view.revisions, Default::default())
                .await?
            {
                match entry {
                    RevisionMapDelta::Added { change, revision } => {
                        if *added.entry(change).or_insert(revision) != revision {
                            divergent.insert(change);
                        }
                    }
                    RevisionMapDelta::Changed { change, .. }
                    | RevisionMapDelta::Removed { change, .. } => {
                        revises_basis.insert(change);
                    }
                }
            }
        }
        if !revises_basis.is_empty() {
            return Err(RepositoryMutationError::FrontierRevisesBasis(
                revises_basis.into_iter().collect(),
            ));
        }
        if !divergent.is_empty() {
            return Err(RepositoryMutationError::DivergentRevisions(
                divergent.into_iter().collect(),
            ));
        }

        let mut records: BTreeMap<ChangeId, (RevisionId, RevisionRecord)> = BTreeMap::new();
        for (change, revision) in &added {
            let record = reader.revision(*revision).await?;
            if record.change_id != *change {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "revision-map change mismatch",
                )
                .into());
            }
            for dependency in &record.dependencies {
                if !added.contains_key(dependency)
                    && maps.get(base.revisions, *dependency).await?.is_none()
                {
                    return Err(wheat_core::RepositoryError::UnknownDependency(*dependency).into());
                }
            }
            records.insert(*change, (*revision, record));
        }
        let selected = records.keys().copied().collect::<BTreeSet<_>>();
        let order = topological_by(&selected, |change| {
            records.get(change).map(|(_, record)| &record.dependencies)
        })?;

        let mut fresh = Fresh::default();
        let mut view = base;
        let mut conflicted = Vec::new();
        for change in &order {
            let (revision, record) = &records[change];
            let edits = self.load_edits(record).await?;
            let next = self
                .apply_step(view, *change, *revision, &edits, &mut fresh)
                .await?;
            if next.unresolved_conflicts != view.unresolved_conflicts {
                conflicted.push(*change);
            }
            view = next;
        }
        let resulting_view = view.id();
        for frontier in frontiers {
            if *frontier != resulting_view {
                fresh.view_edges.insert((*frontier, resulting_view));
            }
        }
        let prepared = self.finish(expected, state, resulting_view, fresh).await?;
        Ok((
            prepared,
            IntegrationOutcome {
                resulting_view,
                composed: order,
                conflicted,
            },
        ))
    }
}
