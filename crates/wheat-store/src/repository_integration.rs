//! Joining frontiers: one view holding the work of several views that grew apart below
//! one basis, such as a label's view after a fetch kept local work beside the remote's.
//!
//! The joined view is the basis with every change the frontiers add composed onto it in
//! canonical order: dependencies first, ties broken by the lowest change id. Changes
//! keep their revisions, so where a frontier's own composition is repeated the
//! applications and views come out the same objects. An application that conflicts is
//! recorded as a durable conflict, as in any composition. Each frontier gets a lineage
//! edge to the joined view, which leaves it the one frontier.
//!
//! Frontiers may hold different revisions of one change. A revision another one
//! descends from is superseded by it. Revisions that went their own ways from a common
//! predecessor are combined by date: the earliest one, with each later one's edit
//! changes since that predecessor laid on top in commit order, becomes a new revision
//! following the earliest. Like any authored revision it must apply cleanly where it
//! lands; if it does not, integration is refused and names the change.

use std::collections::{BTreeMap, BTreeSet};

use wheat_core::{
    ChangeId, EditId, EditRecord, EditTransition, RevisionId, RevisionRecord, ViewId, ViewRecord,
};

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
    /// The revisions authored to combine revisions that went their own ways.
    pub combined: BTreeMap<ChangeId, RevisionId>,
}

/// How one change enters the joined view.
enum Joined {
    /// At a revision some frontier already holds.
    Existing(RevisionId, RevisionRecord),
    /// At a new revision combining several, authored where the change lands.
    Combined(Combination),
}

struct Combination {
    predecessor: RevisionId,
    metadata: wheat_core::RevisionMetadata,
    dependencies: BTreeSet<ChangeId>,
    edits: BTreeMap<EditId, EditRecord>,
    transition: EditTransition,
}

impl Joined {
    fn dependencies(&self) -> &BTreeSet<ChangeId> {
        match self {
            Self::Existing(_, record) => &record.dependencies,
            Self::Combined(combination) => &combination.dependencies,
        }
    }
}

impl<O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'_, O, P> {
    /// Joins `frontiers`, each holding every change of `basis` at the basis's revision,
    /// into one view.
    ///
    /// Refused when a frontier revises or drops a change the basis holds, or when the
    /// revisions of one change cannot be combined. Only what the frontiers add to the
    /// basis is read, so the cost follows the work being joined, not the history below
    /// the basis.
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

        // The revisions each frontier adds to the basis.
        let maps = RevisionMapStore::new(self.objects);
        let mut added: BTreeMap<ChangeId, BTreeSet<RevisionId>> = BTreeMap::new();
        let mut revises_basis = BTreeSet::new();
        for frontier in frontiers {
            let view = reader.view(*frontier).await?;
            for entry in maps
                .delta(base.revisions, view.revisions, Default::default())
                .await?
            {
                match entry {
                    RevisionMapDelta::Added { change, revision } => {
                        added.entry(change).or_default().insert(revision);
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

        let mut joined = BTreeMap::new();
        for (change, revisions) in &added {
            let entry = self.join_revisions(*change, revisions).await?;
            for dependency in entry.dependencies() {
                if !added.contains_key(dependency)
                    && maps.get(base.revisions, *dependency).await?.is_none()
                {
                    return Err(wheat_core::RepositoryError::UnknownDependency(*dependency).into());
                }
            }
            joined.insert(*change, entry);
        }
        let selected = joined.keys().copied().collect::<BTreeSet<_>>();
        let order = topological_by(&selected, |change| {
            joined.get(change).map(Joined::dependencies)
        })?;

        let mut fresh = Fresh::default();
        let mut view = base;
        let mut conflicted = Vec::new();
        let mut combined = BTreeMap::new();
        for change in &order {
            let (revision, edits) = match joined.remove(change).expect("ordered change") {
                Joined::Existing(revision, record) => (revision, self.load_edits(&record).await?),
                Joined::Combined(combination) => {
                    let revision = self
                        .author_combination(*change, combination, &view, &mut fresh)
                        .await?;
                    combined.insert(*change, revision.0);
                    revision
                }
            };
            let next = self
                .apply_step(view, *change, revision, &edits, &mut fresh)
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
                combined,
            },
        ))
    }

    /// The one revision of `change` the joined view holds, given the ones the frontiers
    /// hold.
    async fn join_revisions(
        &self,
        change: ChangeId,
        revisions: &BTreeSet<RevisionId>,
    ) -> Result<Joined> {
        let reader = RepositoryReader::new(self.objects, self.content);
        // Each candidate with its predecessors, nearest first.
        let mut lines: BTreeMap<RevisionId, (RevisionRecord, Vec<RevisionId>)> = BTreeMap::new();
        for revision in revisions {
            let record = reader.revision(*revision).await?;
            if record.change_id != change {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "revision-map change mismatch",
                )
                .into());
            }
            let mut ancestors = Vec::new();
            let mut next = record.predecessor;
            while let Some(predecessor) = next {
                ancestors.push(predecessor);
                next = reader.revision(predecessor).await?.predecessor;
            }
            lines.insert(*revision, (record, ancestors));
        }
        // A revision another one descends from is superseded by it.
        let superseded = lines
            .values()
            .flat_map(|(_, ancestors)| ancestors.iter().copied())
            .collect::<BTreeSet<_>>();
        lines.retain(|revision, _| !superseded.contains(revision));
        if lines.len() == 1 {
            let (revision, (record, _)) = lines.pop_first().expect("one revision");
            return Ok(Joined::Existing(revision, record));
        }

        // The nearest predecessor every remaining line shares.
        let mut lines = lines.into_iter().collect::<Vec<_>>();
        let common = lines[0].1.1.iter().copied().find(|candidate| {
            lines[1..]
                .iter()
                .all(|(_, (_, ancestors))| ancestors.contains(candidate))
        });
        let common_edits = match common {
            Some(common) => reader.revision(common).await?.edits,
            None => BTreeSet::new(),
        };
        lines.sort_by_key(|(revision, (record, _))| {
            (record.metadata.committed_at.seconds, *revision)
        });
        let (earliest, (first, _)) = lines.remove(0);

        let mut selected = first.edits.clone();
        let mut dependencies = first.dependencies.clone();
        let mut metadata = first.metadata.clone();
        for (_, (later, _)) in &lines {
            selected.retain(|edit| later.edits.contains(edit) || !common_edits.contains(edit));
            selected.extend(later.edits.difference(&common_edits).copied());
            dependencies.extend(later.dependencies.iter().copied());
            metadata = later.metadata.clone();
        }
        // When one revision already holds the combined edits, it is the combination.
        if let Some((revision, (record, _))) = lines
            .iter()
            .rev()
            .find(|(_, (record, _))| record.edits == selected)
        {
            return Ok(Joined::Existing(*revision, record.clone()));
        }
        if first.edits == selected {
            return Ok(Joined::Existing(earliest, first));
        }
        let mut edits = BTreeMap::new();
        for edit in &selected {
            edits.insert(*edit, reader.edit(*edit).await?);
        }
        let transition = EditTransition {
            preserved: first.edits.intersection(&selected).copied().collect(),
            added: selected.difference(&first.edits).copied().collect(),
            removed: first.edits.difference(&selected).copied().collect(),
            subsumed: Vec::new(),
            resolutions: Vec::new(),
        };
        Ok(Joined::Combined(Combination {
            predecessor: earliest,
            metadata,
            dependencies,
            edits,
            transition,
        }))
    }

    /// Authors `combination` where `change` lands, on `view`, as a revision that must
    /// apply cleanly there. Returns it with its edits.
    async fn author_combination(
        &self,
        change: ChangeId,
        combination: Combination,
        view: &ViewRecord,
        fresh: &mut Fresh,
    ) -> Result<(RevisionId, BTreeMap<EditId, EditRecord>)> {
        let preliminary = self
            .apply(
                RevisionId::from_digest([0; 32]),
                view,
                &combination.edits,
                fresh,
                false,
            )
            .await?;
        let tree = preliminary
            .clean_tree(&combination.edits)
            .map_err(|error| RepositoryMutationError::RevisionsDoNotCombine {
                change,
                reason: error.to_string(),
            })?;
        let record = RevisionRecord {
            change_id: change,
            metadata: combination.metadata,
            predecessor: Some(combination.predecessor),
            dependencies: combination.dependencies,
            authored_against: view.id(),
            edits: combination.edits.keys().copied().collect(),
            transition: combination.transition,
            resulting_tree: tree,
        };
        self.persist_edits(&combination.edits, fresh).await?;
        let revision = self.put_revision(&record, fresh).await?;
        Ok((revision, combination.edits))
    }
}
