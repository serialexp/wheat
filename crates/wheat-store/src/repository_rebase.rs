//! Appending history from elsewhere below local work, as `git pull --rebase` does.
//!
//! A Git pull brings commits that build on a change the repository already holds,
//! `onto`. They are appended above it as new changes, each depending on the one before.
//! Every change that came after `onto` in canonical order is local work the other side
//! hasn't seen, and it moves above the appended changes:
//!
//! - each root of that work (a change depending on none of the others) gets a new
//!   revision of the same change, depending only on the newest appended change. Its
//!   edits are unchanged and it is authored where its predecessor was, so it replays
//!   cleanly there whatever it meets at its new spot;
//! - the rest keep their revisions and are applied again above it.
//!
//! An application that no longer fits is recorded as a durable conflict, as in any
//! composition. A lineage edge from the old effective view to the result leaves the
//! result the one frontier.

use std::collections::{BTreeMap, BTreeSet};

use wheat_core::{
    ApplicationResultRecord, Change, ChangeId, ChangeObjectId, ChangeProposal, EditTransition,
    RepositoryError, RevisionEdits, RevisionId, RevisionMetadata, RevisionRecord, ViewId,
};

use crate::{
    ApplicationMapStore, ImmutableObjectStore, PublicationStore, RepositoryReader,
    repository_mutation::{
        AuthoringSession, Fresh, PendingRevision, PersistedRepositoryMutator,
        PreparedRepositoryMutation, RepositoryMutationError, descendants, topological,
        validate_initial,
    },
};

type Result<T> = std::result::Result<T, RepositoryMutationError>;

/// What [`PersistedRepositoryMutator::append_and_rebase`] did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebaseOutcome {
    pub resulting_view: ViewId,
    /// The appended changes' revisions, oldest first.
    pub appended: Vec<(ChangeId, RevisionId)>,
    /// The new revision of each root of the local work moved above them.
    pub rebased: BTreeMap<ChangeId, RevisionId>,
    /// The rest of the local work, applied again at its revision, in canonical order.
    pub reapplied: Vec<ChangeId>,
    /// The moved changes whose application conflicted, in canonical order.
    pub conflicted: Vec<ChangeId>,
}

impl<O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'_, O, P> {
    /// Appends `appended` above change `onto` and moves the local work above them; see
    /// the module documentation. `appended` is oldest first: the first depends exactly
    /// on `onto`, each later one exactly on the one before, and none is already in the
    /// repository. `rebased_metadata` gives a moved root's new revision its metadata
    /// from the revision it replaces.
    #[tracing::instrument(level = "debug", skip_all, fields(appended = appended.len()))]
    pub async fn append_and_rebase(
        &self,
        session: AuthoringSession,
        onto: ChangeId,
        appended: Vec<ChangeProposal>,
        rebased_metadata: impl Fn(&RevisionRecord) -> RevisionMetadata,
    ) -> Result<(PreparedRepositoryMutation, RebaseOutcome)> {
        let AuthoringSession {
            expected,
            mut active,
        } = session;
        let Some(position) = active.persisted_order.iter().position(|id| *id == onto) else {
            return Err(RepositoryError::UnknownChange(onto).into());
        };
        let Some(tip) = appended.last().map(|proposal| proposal.id) else {
            return Err(RepositoryMutationError::NothingToAppend);
        };
        let mut previous = onto;
        let mut seen = BTreeSet::new();
        for proposal in &appended {
            if active.revisions.contains_key(&proposal.id) || !seen.insert(proposal.id) {
                return Err(RepositoryError::DuplicateChange(proposal.id).into());
            }
            if proposal.dependencies != BTreeSet::from([previous]) {
                return Err(RepositoryMutationError::AppendedChain(proposal.id));
            }
            validate_initial(&proposal.edits)?;
            previous = proposal.id;
        }

        let moved = active.persisted_order[position + 1..]
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let roots = moved
            .iter()
            .copied()
            .filter(|change| active.revisions[change].1.dependencies.is_disjoint(&moved))
            .collect::<BTreeSet<_>>();

        let mut pending = BTreeMap::new();
        let mut authored_at = BTreeMap::new();
        let changes_in_order = appended
            .iter()
            .map(|proposal| proposal.id)
            .collect::<Vec<_>>();
        let mut changes = Vec::with_capacity(appended.len());
        for proposal in appended {
            changes.push(Change {
                id: proposal.id,
                metadata: proposal.metadata,
            });
            pending.insert(
                proposal.id,
                PendingRevision {
                    predecessor: None,
                    dependencies: proposal.dependencies,
                    metadata: proposal.revision_metadata,
                    edits: proposal.edits,
                },
            );
        }
        for root in &roots {
            let (predecessor, old) = &active.revisions[root];
            let edits = self.load_edits(old).await?;
            let transition = EditTransition {
                preserved: edits.keys().copied().collect(),
                ..EditTransition::default()
            };
            authored_at.insert(*root, (old.authored_against, old.resulting_tree));
            pending.insert(
                *root,
                PendingRevision {
                    predecessor: Some(*predecessor),
                    dependencies: BTreeSet::from([tip]),
                    metadata: rebased_metadata(old),
                    edits: RevisionEdits { edits, transition },
                },
            );
        }

        let mut fresh = Fresh::default();
        let (resulting, revisions) = self
            .compose_authoring_at(&mut active, pending, &authored_at, &mut fresh)
            .await?;
        let objects = crate::ChangeMapStore::new(self.objects);
        for change in changes {
            let bytes = change.canonical_bytes();
            let object = ChangeObjectId::of_canonical_bytes(&bytes);
            self.objects.put_if_absent(object.into(), &bytes).await?;
            fresh.objects += 1;
            fresh.changes.insert(object);
            active.state.changes = objects
                .insert(active.state.changes, change.id, object)
                .await?;
        }
        Self::ensure_continuation_edge(&active, &resulting, &mut fresh);

        let appended = changes_in_order
            .into_iter()
            .map(|change| (change, revisions[&change]))
            .collect();

        let mut reapplied = BTreeSet::new();
        for root in &roots {
            reapplied.extend(descendants(*root, &active.reverse_dependencies));
        }
        let reapplied = topological(&active.revisions, &reapplied)?
            .into_iter()
            .filter(|change| !roots.contains(change))
            .collect::<Vec<_>>();

        let reader = RepositoryReader::new(self.objects, self.content);
        let applications = ApplicationMapStore::new(self.objects);
        let mut conflicted = Vec::new();
        for change in topological(&active.revisions, &moved)? {
            let application = applications
                .get(resulting.applications, change)
                .await?
                .ok_or(RepositoryError::InvalidRepositoryState(
                    "composed view lacks a moved change",
                ))?;
            if matches!(
                reader.application(application).await?.result,
                ApplicationResultRecord::Conflicted { .. }
            ) {
                conflicted.push(change);
            }
        }

        let rebased = roots.iter().map(|root| (*root, revisions[root])).collect();
        let prepared = self
            .finish(expected, active.state, resulting.id(), fresh)
            .await?;
        Ok((
            prepared,
            RebaseOutcome {
                resulting_view: resulting.id(),
                appended,
                rebased,
                reapplied,
                conflicted,
            },
        ))
    }
}
