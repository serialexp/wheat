//! Copy-on-write repository mutation directly over persisted Merkle roots.
//!
//! Mutation deliberately does not reconstruct [`wheat_core::Repository`].  The active revision
//! records are small and are the only history records read eagerly; trees are inspected one edited
//! path at a time and all tree/map/set updates are copy-on-write.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::*;

use crate::{
    ApplicationMapStore, ConflictSetStore, ContentSetStore, ContentStore, ImmutableObjectStore,
    MerkleTreeError, MerkleTreeStore, PublicationStore, RepositoryHead, RepositoryPublicationError,
    RepositoryPublisher, RepositoryReader, RetainedSetStore, RevisionMapStore, TreeEdit,
};

#[derive(Debug, Error)]
pub enum RepositoryMutationError {
    #[error(transparent)]
    Publication(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Model(#[from] RepositoryError),
    #[error(transparent)]
    Revisions(#[from] crate::RevisionMapError),
    #[error(transparent)]
    Changes(#[from] crate::ChangeMapError),
    #[error(transparent)]
    Applications(#[from] crate::ApplicationMapError),
    #[error(transparent)]
    Collections(#[from] crate::CollectionError),
    #[error(transparent)]
    ContentMap(#[from] crate::ContentMapError),
    #[error(transparent)]
    ViewLineage(#[from] crate::ViewLineageError),
    #[error(transparent)]
    Trees(#[from] MerkleTreeError),
    #[error(transparent)]
    Objects(#[from] crate::ObjectStoreError),
    #[error(transparent)]
    Commit(#[from] crate::CommitWriterError),
    #[error("mutation expected {expected:?}, but current head is {actual:?}")]
    Stale {
        expected: RepositoryHead,
        actual: RepositoryHead,
    },
    #[error("frontier creation requires a proposal with no explicit dependencies")]
    FrontierDependencies,
    #[error("integration needs at least one frontier")]
    NoFrontiers,
    #[error(
        "the revisions of change {change} the frontiers hold do not apply on top of one \
         another: {reason}"
    )]
    RevisionsDoNotCombine { change: ChangeId, reason: String },
    #[error(
        "the frontiers hold {count} revisions of change {change} that went their own ways, \
         and at most {} combine at once; integrate fewer frontiers at a time (--frontier)",
        wheat_core::MAX_COMBINES + 1
    )]
    TooManyRevisions { change: ChangeId, count: usize },
}

type Result<T> = std::result::Result<T, RepositoryMutationError>;

/// A sealed, boundedly validated state transition.
pub struct PreparedRepositoryMutation {
    expected: RepositoryHead,
    state: RepositoryStateId,
    resulting_view: ViewId,
    objects: usize,
    content: usize,
    _sealed: (),
}

impl PreparedRepositoryMutation {
    #[must_use]
    pub fn state(&self) -> RepositoryStateId {
        self.state
    }
    #[must_use]
    pub fn resulting_view(&self) -> ViewId {
        self.resulting_view
    }
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.objects
    }
    #[must_use]
    pub fn content_count(&self) -> usize {
        self.content
    }
}

pub struct PersistedRepositoryMutator<'a, O, P> {
    pub(crate) objects: &'a O,
    pub(crate) content: &'a ContentStore,
    pub(crate) publications: &'a P,
}

/// A conflict-resolution intent over the paths whose edits failed in one exact application.
///
/// A missing value deletes the path. Paths omitted from `resolved` retain the conflicted
/// application's partial-tree value, matching a hydrated resolution tree which changed only the
/// listed paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedConflictResolution {
    pub conflict_revision: RevisionId,
    pub application: ApplicationId,
    pub resolved: BTreeMap<String, Option<FileValue>>,
}

/// A repository opened once by [`PersistedRepositoryMutator::open_authoring`]. It is read with
/// [`PersistedRepositoryMutator::later_edits`] and consumed by
/// [`PersistedRepositoryMutator::revise_changes_in`], which publishes only if the head is still
/// the one it was opened at.
pub struct AuthoringSession {
    pub(crate) expected: RepositoryHead,
    pub(crate) active: Active,
}

pub(crate) struct Active {
    pub(crate) state: RepositoryStateRecord,
    pub(crate) effective: ViewRecord,
    pub(crate) revisions: BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    /// The revision identities and canonical order represented by `effective` before this mutation.
    persisted_revisions: BTreeMap<ChangeId, RevisionId>,
    pub(crate) persisted_order: Vec<ChangeId>,
    reverse_dependencies: BTreeMap<ChangeId, BTreeSet<ChangeId>>,
}

/// How [`PersistedRepositoryMutator::apply`] replays a revision's edits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Replay {
    /// Authoring an ordinary revision where it lands: every edit must apply, and only
    /// generated content and trees are written.
    Author,
    /// Authoring a combined revision: its inputs' edits may already be satisfied where
    /// it lands, as for a reapplied revision, but none may fail.
    AuthorCombined,
    /// Applying a revision: an edit may already be satisfied, and a conflict is written.
    Apply,
}

impl Replay {
    /// Whether an edit whose result is already in place counts as applied.
    #[must_use]
    pub(crate) fn allows_already_satisfied(self) -> bool {
        self != Self::Author
    }
}

/// A replay of a revision's edits: its result, the evidence for each edit that applied,
/// and the evidence for each edit that failed (the conflict's edits; empty when the
/// result is clean).
pub(crate) struct AppliedEdits {
    pub(crate) result: ApplicationResultRecord,
    pub(crate) applied: BTreeMap<EditId, AppliedEditEvidence>,
    pub(crate) failed: BTreeMap<EditId, FailedEditEvidence>,
}

impl AppliedEdits {
    /// The clean resulting tree, or [`RepositoryError::InvalidEdits`] naming each edit
    /// that failed and why.
    pub(crate) fn clean_tree(&self, edits: &BTreeMap<EditId, EditRecord>) -> Result<TreeId> {
        match self.result {
            ApplicationResultRecord::Clean { tree } => Ok(tree),
            ApplicationResultRecord::Conflicted { .. } => Err(RepositoryError::InvalidEdits(
                RejectedEdit::collect(edits, &self.failed),
            )
            .into()),
        }
    }
}

/// A revision waiting to be authored at its spot in the canonical composition.
pub(crate) struct PendingRevision {
    pub(crate) predecessor: Option<RevisionId>,
    pub(crate) dependencies: BTreeSet<ChangeId>,
    pub(crate) metadata: RevisionMetadata,
    pub(crate) edits: RevisionEdits,
}

#[derive(Default)]
pub(crate) struct Fresh {
    pub(crate) changes: BTreeSet<ChangeObjectId>,
    pub(crate) view_edges: BTreeSet<(ViewId, ViewId)>,
    pub(crate) views: BTreeSet<ViewId>,
    pub(crate) revisions: BTreeSet<RevisionId>,
    pub(crate) applications: BTreeSet<ApplicationId>,
    pub(crate) content: BTreeSet<ContentId>,
    pub(crate) objects: usize,
}

impl<'a, O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'a, O, P> {
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore, publications: &'a P) -> Self {
        Self {
            objects,
            content,
            publications,
        }
    }

    pub async fn create(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        proposal: ChangeProposal,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected, basis_view, effective_view).await?;
        self.create_opened(expected, active, proposal).await
    }

    /// Create a change on the current graph frontier.
    ///
    /// The proposal must not provide explicit dependencies. Its dependencies are derived from the
    /// current persisted graph as every active change with no dependents. This makes the new change
    /// follow the complete active graph while keeping its direct dependency set to the frontier.
    pub async fn create_on_frontier(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        mut proposal: ChangeProposal,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected, basis_view, effective_view).await?;
        if !proposal.dependencies.is_empty() {
            return Err(RepositoryMutationError::FrontierDependencies);
        }
        proposal.dependencies = active
            .reverse_dependencies
            .iter()
            .filter_map(|(change, dependents)| dependents.is_empty().then_some(*change))
            .collect();
        self.create_opened(expected, active, proposal).await
    }

    async fn create_opened(
        &self,
        expected: RepositoryHead,
        mut active: Active,
        proposal: ChangeProposal,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        if active.revisions.contains_key(&proposal.id) {
            return Err(RepositoryError::DuplicateChange(proposal.id).into());
        }
        for dependency in &proposal.dependencies {
            if !active.revisions.contains_key(dependency) {
                return Err(RepositoryError::UnknownDependency(*dependency).into());
            }
        }
        validate_initial(&proposal.edits)?;
        let mut fresh = Fresh::default();
        let pending = BTreeMap::from([(
            proposal.id,
            PendingRevision {
                predecessor: None,
                dependencies: proposal.dependencies,
                metadata: proposal.revision_metadata,
                edits: proposal.edits,
            },
        )]);
        let (resulting, revised) = self
            .compose_authoring(&mut active, pending, &mut fresh)
            .await?;
        let revision_id = revised[&proposal.id];
        let change = Change {
            id: proposal.id,
            metadata: proposal.metadata,
        };
        let change_id = ChangeObjectId::of_canonical_bytes(&change.canonical_bytes());
        self.objects
            .put_if_absent(change_id.into(), &change.canonical_bytes())
            .await?;
        fresh.objects += 1;
        fresh.changes.insert(change_id);
        active.state.changes = crate::ChangeMapStore::new(self.objects)
            .insert(active.state.changes, proposal.id, change_id)
            .await?;
        // Ensure the lineage records a continuation edge from the previous effective
        // view to the new result.  Composition rebuilds the composed view from the base
        // and records only intra-compose edges (base → result), so when the effective
        // view was already a descendant of the base (e.g. from a prior `change new`),
        // its vertex becomes an orphaned leaf unless we connect it explicitly.
        Self::ensure_continuation_edge(&active, &resulting, &mut fresh);
        let outcome = RevisionOutcome {
            revision_id,
            resulting_view: resulting.id(),
            reapplied: Vec::new(),
        };
        let prepared = self
            .finish(expected, active.state, resulting.id(), fresh)
            .await?;
        Ok((prepared, outcome))
    }

    pub async fn resolve_conflict(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        resolution: PersistedConflictResolution,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected, basis_view, effective_view).await?;
        let reader = RepositoryReader::new(self.objects, self.content);
        let revision = reader.revision(resolution.conflict_revision).await?;
        let change = revision.change_id;
        let Some((current, predecessor)) = active.revisions.get(&change) else {
            return Err(RepositoryError::UnknownChange(change).into());
        };
        if *current != resolution.conflict_revision {
            return Err(RepositoryError::StaleRevision(change).into());
        }
        let predecessor_edits = self.load_edits(predecessor).await?;

        let applications = ApplicationMapStore::new(self.objects);
        let current_application = applications
            .get(active.effective.applications, change)
            .await?;
        if current_application != Some(resolution.application) {
            return Err(RepositoryError::NotConflicted(resolution.conflict_revision).into());
        }
        let application = reader.application(resolution.application).await?;
        if application.revision_id != resolution.conflict_revision {
            return Err(RepositoryError::NotConflicted(resolution.conflict_revision).into());
        }
        let ApplicationResultRecord::Conflicted {
            partial_tree,
            conflict,
        } = application.result
        else {
            return Err(RepositoryError::NotConflicted(resolution.conflict_revision).into());
        };
        let conflict = reader.conflict(conflict).await?;
        let conflict_paths = conflict
            .edits
            .keys()
            .filter_map(|id| predecessor_edits.get(id))
            .map(|edit| edit.path.as_str())
            .collect::<BTreeSet<_>>();

        let trees = MerkleTreeStore::new(self.objects);
        let target = reader.view(application.target_view).await?;
        let mut replacements = BTreeMap::new();
        // `resolved` is keyed by path, so each tree is resolved in one sorted traversal.
        let resolved_paths = resolution
            .resolved
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let partial_values = trees.lookup_many(partial_tree, &resolved_paths).await?;
        let before_values = trees.lookup_many(target.tree, &resolved_paths).await?;
        let content_error =
            |error: crate::ContentStoreError| RepositoryError::ContentIo(error.to_string());
        let mut captured_content = crate::ContentWriteBatch::new(self.content);
        for (((path, after_value), partial_value), before_value) in resolution
            .resolved
            .iter()
            .zip(partial_values)
            .zip(before_values)
        {
            if partial_value == *after_value {
                continue;
            }
            if !conflict_paths.contains(path.as_str()) {
                return Err(RepositoryError::InvalidConflictResolution.into());
            }
            // Resolve and authenticate content before constructing or persisting metadata which
            // refers to it. This also supplies capture without hydrating either tree.
            let before_bytes = before_value
                .map(|value| {
                    self.content
                        .resolve(value.content)
                        .map_err(|error| RepositoryError::ContentIo(error.to_string()))
                })
                .transpose()?;
            let after_bytes = after_value
                .map(|value| {
                    self.content
                        .resolve(value.content)
                        .map_err(|error| RepositoryError::ContentIo(error.to_string()))
                })
                .transpose()?;
            let before = before_value
                .zip(before_bytes.as_ref())
                .map(|(value, content)| wheat_core::edit_capture::FileSnapshot {
                    kind: value.kind,
                    bytes: content.bytes(),
                });
            let after = after_value
                .zip(after_bytes.as_ref())
                .map(|(value, content)| wheat_core::edit_capture::FileSnapshot {
                    kind: value.kind,
                    bytes: content.bytes(),
                });
            let captured = wheat_core::edit_capture::capture_edit(
                path,
                before,
                after,
                None,
                wheat_core::guarded::Limits::default(),
            )
            .map_err(|error| RepositoryError::EditApplication(format!("capture: {error:?}")))?;
            for content in captured.content.into_values() {
                captured_content
                    .put_verified(content)
                    .map_err(content_error)?;
            }
            replacements.extend(captured.edits);
        }
        captured_content.commit().map_err(content_error)?;

        let removed = conflict.edits.keys().copied().collect::<BTreeSet<_>>();
        let preserved = predecessor_edits
            .keys()
            .filter(|id| !removed.contains(id))
            .copied()
            .collect::<BTreeSet<_>>();
        let replacement_edits = replacements.keys().copied().collect::<BTreeSet<_>>();
        let mut edits = predecessor_edits.clone();
        edits.retain(|id, _| preserved.contains(id));
        edits.extend(replacements);
        let revision_edits = RevisionEdits {
            edits,
            transition: EditTransition {
                preserved,
                added: replacement_edits.clone(),
                removed,
                resolutions: vec![ConflictResolutionEvidence {
                    application: resolution.application,
                    conflicted_edits: conflict.edits.keys().copied().collect(),
                    replacement_edits,
                }],
                ..EditTransition::default()
            },
        };
        self.revise(
            expected,
            basis_view,
            effective_view,
            change,
            resolution.conflict_revision,
            RevisionMetadata::default(),
            revision_edits,
        )
        .await
    }

    pub async fn revise(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        change: ChangeId,
        predecessor: RevisionId,
        metadata: RevisionMetadata,
        edits: RevisionEdits,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let request = RevisionRequest {
            predecessor,
            metadata,
            edits,
        };
        let (prepared, outcome) = self
            .revise_changes(
                expected,
                basis_view,
                effective_view,
                BTreeMap::from([(change, request)]),
            )
            .await?;
        let outcome = RevisionOutcome {
            revision_id: outcome.revisions[&change],
            resulting_view: outcome.resulting_view,
            reapplied: outcome.reapplied,
        };
        Ok((prepared, outcome))
    }

    /// Revises several changes in one publication.
    ///
    /// Each new revision is authored at its change's spot: the view after every change before it
    /// in canonical order, including revisions made earlier in this same call. That is what the
    /// workspace shows underneath the change, so work written on top of earlier siblings relies
    /// on them; reapplying the change elsewhere reports a conflict if its edits no longer fit.
    pub async fn revise_changes(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        requests: BTreeMap<ChangeId, RevisionRequest>,
    ) -> Result<(PreparedRepositoryMutation, RevisionsOutcome)> {
        let session = self
            .open_authoring(expected, basis_view, effective_view)
            .await?;
        self.revise_changes_in(session, requests).await
    }

    /// Opens the repository once for an authoring command that reads it before revising it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn open_authoring(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
    ) -> Result<AuthoringSession> {
        Ok(AuthoringSession {
            expected,
            active: self.open(expected, basis_view, effective_view).await?,
        })
    }

    /// [`Self::revise_changes`] on an already opened repository.
    #[tracing::instrument(level = "debug", skip_all, fields(changes = requests.len()))]
    pub async fn revise_changes_in(
        &self,
        session: AuthoringSession,
        requests: BTreeMap<ChangeId, RevisionRequest>,
    ) -> Result<(PreparedRepositoryMutation, RevisionsOutcome)> {
        if requests.is_empty() {
            return Err(RepositoryError::InvalidEditTransition(
                "a revision needs at least one change",
            )
            .into());
        }
        let AuthoringSession {
            expected,
            mut active,
        } = session;
        let mut pending = BTreeMap::new();
        for (change, request) in requests {
            let Some((current, old)) = active.revisions.get(&change) else {
                return Err(RepositoryError::UnknownChange(change).into());
            };
            if *current != request.predecessor {
                return Err(RepositoryError::StaleRevision(change).into());
            }
            let dependencies = old.dependencies.clone();
            let old_edits = self.load_edits(old).await?;
            self.validate_revision_transition(
                change,
                request.predecessor,
                old_edits,
                &request.edits,
                &active,
            )
            .await?;
            pending.insert(
                change,
                PendingRevision {
                    predecessor: Some(request.predecessor),
                    dependencies,
                    metadata: request.metadata,
                    edits: request.edits,
                },
            );
        }
        let revised_changes = pending.keys().copied().collect::<BTreeSet<_>>();
        let mut fresh = Fresh::default();
        let (resulting, revisions) = self
            .compose_authoring(&mut active, pending, &mut fresh)
            .await?;
        Self::ensure_continuation_edge(&active, &resulting, &mut fresh);
        let mut reapplied = BTreeSet::new();
        for change in &revised_changes {
            reapplied.extend(descendants(*change, &active.reverse_dependencies));
        }
        let reapplied = topological(&active.revisions, &reapplied)?
            .into_iter()
            .filter(|id| !revised_changes.contains(id))
            .collect();
        let outcome = RevisionsOutcome {
            revisions,
            resulting_view: resulting.id(),
            reapplied,
        };
        let prepared = self
            .finish(expected, active.state, resulting.id(), fresh)
            .await?;
        Ok((prepared, outcome))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn publish(&self, prepared: PreparedRepositoryMutation) -> Result<RepositoryHead> {
        let actual = self
            .publications
            .read()
            .map_err(RepositoryPublicationError::from)?;
        if actual != prepared.expected {
            return Err(RepositoryMutationError::Stale {
                expected: prepared.expected,
                actual,
            });
        }
        let reader = RepositoryReader::new(self.objects, self.content);
        let old = reader.envelope(prepared.expected.envelope).await?;
        let envelope = RepositoryPublisher::new(self.objects, self.content, self.publications)
            .prepare_envelope_unchecked(prepared.state, old.labels)
            .await?;
        let proposed = prepared
            .expected
            .next(envelope)
            .map_err(RepositoryPublicationError::from)?;
        self.publications
            .compare_and_advance(prepared.expected, proposed)
            .map_err(RepositoryPublicationError::from)?;
        Ok(proposed)
    }

    #[tracing::instrument(name = "open_repository", level = "debug", skip_all)]
    pub(crate) async fn open(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
    ) -> Result<Active> {
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
        for view in [basis_view, effective_view] {
            if !retained.contains(state.retained_views, view.into()).await? {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "mutation view is not retained",
                )
                .into());
            }
        }
        let effective = reader.view(effective_view).await?;
        let mut revisions = BTreeMap::new();
        let mut cursor = RevisionMapStore::new(self.objects)
            .cursor(effective.revisions)
            .await?;
        while let Some((change, id)) = cursor.next().await? {
            let record = reader.revision(id).await?;
            if record.change_id != change {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "revision-map change mismatch",
                )
                .into());
            }
            revisions.insert(change, (id, record));
        }
        let persisted_revisions = revisions
            .iter()
            .map(|(change, (revision, _))| (*change, *revision))
            .collect();
        let mut active = Active {
            state,
            effective,
            revisions,
            persisted_revisions,
            persisted_order: Vec::new(),
            reverse_dependencies: BTreeMap::new(),
        };
        active.rebuild_graph();
        active.persisted_order = topological(
            &active.revisions,
            &active.revisions.keys().copied().collect(),
        )?;
        if effective_view != basis_view && !self.descends(&active, basis_view).await? {
            return Err(RepositoryPublicationError::InvalidReferences(
                "effective view does not descend from basis view",
            )
            .into());
        }
        Ok(active)
    }

    /// Whether `active.effective` descends from `basis` in the view lineage.
    ///
    /// The effective view records, for each applied change, the view that change was
    /// applied onto. In canonical order those views are the path the effective view was
    /// composed along. When the basis is on that path and every later hop is a lineage
    /// edge, descent is proven with one edge lookup per change, whatever the size of the
    /// history. Anything else (say, a view reached another way) falls back to the forward
    /// search, which is exact but visits the history under the basis.
    #[tracing::instrument(level = "debug", skip_all, fields(changes = active.persisted_order.len()))]
    async fn descends(&self, active: &Active, basis: ViewId) -> Result<bool> {
        let lineage = crate::ViewLineageMapStore::new(self.objects);
        let reader = RepositoryReader::new(self.objects, self.content);
        let mut applications = BTreeMap::new();
        let mut cursor = ApplicationMapStore::new(self.objects)
            .cursor(active.effective.applications)
            .await?;
        while let Some((change, application)) = cursor.next().await? {
            applications.insert(change, application);
        }
        let mut path = Vec::with_capacity(active.persisted_order.len() + 1);
        for change in &active.persisted_order {
            let Some(application) = applications.get(change) else {
                path.clear();
                break;
            };
            path.push(reader.application(*application).await?.target_view);
        }
        if let Some(start) = path.iter().position(|view| *view == basis) {
            path.push(active.effective.id());
            if lineage
                .follows_path(active.state.view_lineage, &path[start..])
                .await?
            {
                return Ok(true);
            }
        }
        Ok(lineage
            .is_descendant(
                active.state.view_lineage,
                basis,
                active.effective.id(),
                crate::ViewLineageLimits::default(),
            )
            .await?)
    }

    /// Authors every pending revision at its change's spot while composing all active changes.
    ///
    /// A change's spot is the view after every change before it in canonical order. Pending
    /// changes that are new join the canonical order through their dependencies; revised changes
    /// keep theirs, so the order of the others is unchanged. Each authored revision's edits must
    /// apply cleanly at its spot. Returns the composed view and the new revision of each pending
    /// change, which also replaces it in `active`.
    #[tracing::instrument(level = "debug", skip_all, fields(pending = pending.len()))]
    pub(crate) async fn compose_authoring(
        &self,
        active: &mut Active,
        mut pending: BTreeMap<ChangeId, PendingRevision>,
        fresh: &mut Fresh,
    ) -> Result<(ViewRecord, BTreeMap<ChangeId, RevisionId>)> {
        let all = active
            .revisions
            .keys()
            .chain(pending.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        let order = topological_by(&all, |change| {
            pending
                .get(change)
                .map(|revision| &revision.dependencies)
                .or_else(|| active.revisions.get(change).map(|(_, r)| &r.dependencies))
        })?;
        let prefix = unchanged_prefix(active, &order, &pending);
        let mut view = self.prefix_view(active, prefix).await?;
        let mut authored = BTreeMap::new();
        for change in order.into_iter().skip(prefix) {
            let (revision_id, generation, edits) = if let Some(revision) = pending.remove(&change) {
                let preliminary = self
                    .apply(&view, &revision.edits.edits, fresh, Replay::Author)
                    .await?;
                let tree = preliminary.clean_tree(&revision.edits.edits)?;
                let generation = match revision.predecessor {
                    None => 0,
                    Some(predecessor) => {
                        let parent = match active.revisions.get(&change) {
                            Some((id, record)) if *id == predecessor => record.generation,
                            _ => {
                                RepositoryReader::new(self.objects, self.content)
                                    .revision(predecessor)
                                    .await?
                                    .generation
                            }
                        };
                        generation_after([parent])?
                    }
                };
                let record = RevisionRecord {
                    change_id: change,
                    metadata: revision.metadata,
                    predecessor: revision.predecessor,
                    combines: BTreeSet::new(),
                    generation,
                    dependencies: revision.dependencies,
                    authored_against: view.id(),
                    edits: revision.edits.edits.keys().copied().collect(),
                    transition: revision.edits.transition,
                    resulting_tree: tree,
                };
                self.persist_edits(&revision.edits.edits, fresh).await?;
                let revision_id = self.put_revision(&record, fresh).await?;
                active.revisions.insert(change, (revision_id, record));
                authored.insert(change, revision_id);
                (revision_id, generation, revision.edits.edits)
            } else {
                let (revision_id, revision) = &active.revisions[&change];
                (
                    *revision_id,
                    revision.generation,
                    self.load_edits(revision).await?,
                )
            };
            view = self
                .apply_step(view, change, revision_id, generation, &edits, fresh)
                .await?;
        }
        active.rebuild_graph();
        Ok((view, authored))
    }

    /// The view after the first `prefix` changes of the persisted canonical order.
    ///
    /// The target of the next persisted application is precisely that intermediate view, so no
    /// edit or tree in the prefix needs to be read. That holds for an empty prefix too: the
    /// basis may itself hold changes, such as a label's view, and is then not the view below
    /// them.
    #[tracing::instrument(level = "debug", skip_all, fields(prefix))]
    async fn prefix_view(&self, active: &Active, prefix: usize) -> Result<ViewRecord> {
        if prefix == active.persisted_order.len() {
            return Ok(active.effective);
        }
        let next = active.persisted_order[prefix];
        let application = ApplicationMapStore::new(self.objects)
            .get(active.effective.applications, next)
            .await?
            .ok_or(RepositoryError::InvalidRepositoryState(
                "effective view lacks canonical application",
            ))?;
        let reader = RepositoryReader::new(self.objects, self.content);
        let application = reader.application(application).await?;
        Ok(reader.view(application.target_view).await?)
    }

    /// Applies one change's revision, of `generation`, onto `view` and records the
    /// resulting view.
    #[tracing::instrument(level = "debug", skip_all, fields(change = %change))]
    pub(crate) async fn apply_step(
        &self,
        view: ViewRecord,
        change: ChangeId,
        revision_id: RevisionId,
        generation: u64,
        edits: &BTreeMap<EditId, EditRecord>,
        fresh: &mut Fresh,
    ) -> Result<ViewRecord> {
        let order =
            view.order
                .then(change, generation)
                .ok_or(RepositoryError::InvalidRepositoryState(
                    "view weight overflow",
                ))?;
        let replay = self.apply(&view, edits, fresh, Replay::Apply).await?;
        let application = ApplicationRecord {
            revision_id,
            target_view: view.id(),
            order: RepositoryReader::new(self.objects, self.content)
                .application_order(&view, change)
                .await?,
            result: replay.result,
            applied: replay.applied,
        };
        let application_id = application.id();
        self.objects
            .put_if_absent(application_id.into(), &application.canonical_bytes())
            .await?;
        fresh.objects += 1;
        fresh.applications.insert(application_id);
        let tree = match application.result {
            ApplicationResultRecord::Clean { tree } => tree,
            ApplicationResultRecord::Conflicted { partial_tree, .. } => partial_tree,
        };
        let revision_root = RevisionMapStore::new(self.objects)
            .insert(view.revisions, change, revision_id)
            .await?;
        let application_root = ApplicationMapStore::new(self.objects)
            .insert(view.applications, change, application_id)
            .await?;
        let conflict_root = match application.result {
            ApplicationResultRecord::Conflicted { conflict, .. } => {
                ConflictSetStore::new(self.objects)
                    .insert(view.unresolved_conflicts, conflict)
                    .await?
            }
            ApplicationResultRecord::Clean { .. } => view.unresolved_conflicts,
        };
        let content = self
            .update_visible_content(view.required_content, edits, tree, &view, fresh)
            .await?;
        let parent = view.id();
        let next = ViewRecord {
            tree,
            revisions: revision_root,
            applications: application_root,
            unresolved_conflicts: conflict_root,
            required_content: content,
            order,
        };
        self.put_view(&next, fresh).await?;
        if parent != next.id() {
            fresh.view_edges.insert((parent, next.id()));
        }
        Ok(next)
    }

    /// Record a lineage edge from the prior effective view to the new composed result so that the
    /// prior effective view is no longer a leaf frontier.
    ///
    /// Composition rebuilds the result from the view below the first change that differs, and only
    /// records edges between the views it composes. That view may be below the effective view, and
    /// even below the basis when a change the basis holds is revised, which would leave the result
    /// outside the basis's descendants and the old effective view an orphaned leaf. This explicit
    /// continuation edge connects the two, maintaining a single-frontier chain.
    fn ensure_continuation_edge(active: &Active, resulting: &ViewRecord, fresh: &mut Fresh) {
        let effective_id = active.effective.id();
        let resulting_id = resulting.id();
        if effective_id != resulting_id {
            fresh.view_edges.insert((effective_id, resulting_id));
        }
    }

    #[tracing::instrument(level = "debug", skip_all, fields(edits = revision.edits.len()))]
    pub(crate) async fn load_edits(
        &self,
        revision: &RevisionRecord,
    ) -> Result<BTreeMap<EditId, EditRecord>> {
        let reader = RepositoryReader::new(self.objects, self.content);
        let mut edits = BTreeMap::new();
        for id in &revision.edits {
            edits.insert(*id, reader.edit(*id).await?);
        }
        Ok(edits)
    }

    #[tracing::instrument(name = "apply_revision", level = "debug", skip_all, fields(edits = edits.len()))]
    pub(crate) async fn apply(
        &self,
        target: &ViewRecord,
        edits: &BTreeMap<EditId, EditRecord>,
        fresh: &mut Fresh,
        replay: Replay,
    ) -> Result<AppliedEdits> {
        let trees = MerkleTreeStore::new(self.objects);
        let paths = edit_paths(edits);
        let mut flat = Tree::new();
        for (path, value) in paths
            .iter()
            .zip(trees.lookup_many(target.tree, &paths).await?)
        {
            if let Some(value) = value {
                flat.insert((*path).to_owned(), value);
            }
        }
        let before = flat.clone();
        let limits = wheat_core::edit_application::ApplicationLimits {
            allow_already_satisfied: replay.allows_already_satisfied(),
            ..Default::default()
        };
        let output = wheat_core::edit_application::apply_edits(
            &flat,
            edits.iter().map(|(id, edit)| (*id, edit.clone())),
            |id| self.content.resolve(id),
            limits,
        )
        .map_err(|error| RepositoryError::EditApplication(format!("{error:?}")))?;
        // One application can generate a blob per guarded path; retain them in bounded batches
        // rather than one durable write each.
        let content_error =
            |error: crate::ContentStoreError| RepositoryError::ContentIo(error.to_string());
        let mut generated = crate::ContentWriteBatch::new(self.content);
        for (id, content) in &output.generated_content {
            generated
                .put_verified(content.borrowed())
                .map_err(content_error)?;
            fresh.content.insert(*id);
        }
        generated.commit().map_err(content_error)?;
        let mut tree_edits = Vec::new();
        for path in before
            .keys()
            .chain(output.tree.keys())
            .collect::<BTreeSet<_>>()
        {
            if before.get(path) != output.tree.get(path) {
                tree_edits.push(TreeEdit {
                    path: path_components(path),
                    value: output.tree.get(path).copied(),
                });
            }
        }
        // A final-tree update, so a directory that becomes a file (or the reverse)
        // in one revision works: its removals and its new entry land together.
        let tree = trees.apply_final_edits(target.tree, &tree_edits).await?;
        let mut applied = BTreeMap::new();
        let mut failed = BTreeMap::new();
        for (id, outcome) in output.outcomes {
            let target_value = before.get(&edits[&id].path).copied();
            match outcome {
                wheat_core::edit_application::ApplicationOutcome::Applied { read, write } => {
                    applied.insert(
                        id,
                        AppliedEditEvidence {
                            target: target_value,
                            read: read.map(byte_range),
                            write: write.map(byte_range),
                        },
                    );
                }
                wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { read } => {
                    applied.insert(
                        id,
                        AppliedEditEvidence {
                            target: target_value,
                            read: read.map(byte_range),
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
                    if let Some(value) = target_value {
                        fresh.content.insert(value.content);
                    }
                    failed.insert(
                        id,
                        FailedEditEvidence {
                            target: target_value,
                            reason,
                        },
                    );
                }
            }
        }
        let (result, failed) = if failed.is_empty() {
            (ApplicationResultRecord::Clean { tree }, failed)
        } else {
            let conflict = Conflict::new(target.id(), failed);
            if replay == Replay::Apply {
                self.objects
                    .put_if_absent(conflict.id.into(), &conflict.canonical_bytes())
                    .await?;
                fresh.objects += 1;
            }
            (
                ApplicationResultRecord::Conflicted {
                    partial_tree: tree,
                    conflict: conflict.id,
                },
                conflict.edits,
            )
        };
        Ok(AppliedEdits {
            result,
            applied,
            failed,
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn update_visible_content(
        &self,
        old: ContentMapId,
        edits: &BTreeMap<EditId, EditRecord>,
        new_tree: TreeId,
        old_view: &ViewRecord,
        fresh: &mut Fresh,
    ) -> Result<ContentMapId> {
        let trees = MerkleTreeStore::new(self.objects);
        let paths = edit_paths(edits);
        let befores = trees.lookup_many(old_view.tree, &paths).await?;
        let afters = trees.lookup_many(new_tree, &paths).await?;
        let mut touched = Vec::new();
        for (before, after) in befores.into_iter().zip(afters) {
            if before != after {
                if let Some(value) = after {
                    fresh.content.insert(value.content);
                }
                touched.push((before, after));
            }
        }
        // The map counts the tree entries naming each blob, so a blob still named at an
        // untouched path keeps a positive count after its touched path changes.
        Ok(crate::commit_content::update_content(self.objects, old, touched).await?)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn put_view(&self, view: &ViewRecord, fresh: &mut Fresh) -> Result<()> {
        self.objects
            .put_if_absent(view.id().into(), &view.canonical_bytes())
            .await?;
        fresh.objects += 1;
        fresh.views.insert(view.id());
        Ok(())
    }
    pub(crate) async fn put_revision(
        &self,
        revision: &RevisionRecord,
        fresh: &mut Fresh,
    ) -> Result<RevisionId> {
        let id = revision.id();
        self.objects
            .put_if_absent(id.into(), &revision.canonical_bytes())
            .await?;
        fresh.objects += 1;
        fresh.revisions.insert(id);
        Ok(id)
    }
    #[tracing::instrument(level = "debug", skip_all, fields(edits = edits.len()))]
    pub(crate) async fn persist_edits(
        &self,
        edits: &BTreeMap<EditId, EditRecord>,
        fresh: &mut Fresh,
    ) -> Result<()> {
        for (id, edit) in edits {
            self.objects
                .put_if_absent((*id).into(), &edit.canonical_bytes())
                .await?;
            fresh.objects += 1;
            match &edit.operation {
                EditOperation::GuardedText {
                    before,
                    expected,
                    replacement,
                    after,
                    ..
                } => fresh
                    .content
                    .extend([*before, *expected, *replacement, *after]),
                EditOperation::ExactFile {
                    expected,
                    replacement,
                } => fresh
                    .content
                    .extend(expected.iter().chain(replacement).map(|v| v.content)),
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn validate_revision_transition(
        &self,
        change: ChangeId,
        predecessor: RevisionId,
        old: BTreeMap<EditId, EditRecord>,
        edits: &RevisionEdits,
        active: &Active,
    ) -> Result<()> {
        edits
            .validate()
            .map_err(|_| RepositoryError::InvalidEditTransition("invalid local edit selection"))?;
        let old_ids = old.keys().copied().collect::<BTreeSet<_>>();
        if old_ids
            != edits
                .transition
                .preserved
                .union(&edits.transition.removed)
                .copied()
                .collect()
            || edits
                .transition
                .preserved
                .iter()
                .any(|id| old.get(id) != edits.edits.get(id))
        {
            return Err(
                RepositoryError::InvalidEditTransition("predecessor edit partition").into(),
            );
        }
        let applications = ApplicationMapStore::new(self.objects);
        let reader = RepositoryReader::new(self.objects, self.content);
        for evidence in &edits.transition.resolutions {
            let current = applications
                .get(active.effective.applications, change)
                .await?;
            if current != Some(evidence.application) {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution application is not current for the change",
                )
                .into());
            }
            let app = reader.application(evidence.application).await?;
            if app.revision_id != predecessor {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution application is not current for the change",
                )
                .into());
            }
            let ApplicationResultRecord::Conflicted { conflict, .. } = app.result else {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution application is not conflicted",
                )
                .into());
            };
            let conflict = reader.conflict(conflict).await?;
            if !evidence
                .conflicted_edits
                .is_subset(&conflict.edits.keys().copied().collect())
            {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution conflicted edits were not failed",
                )
                .into());
            }
        }
        Ok(())
    }

    #[tracing::instrument(name = "finish_mutation", level = "debug", skip_all)]
    pub(crate) async fn finish(
        &self,
        expected: RepositoryHead,
        old: RepositoryStateRecord,
        resulting_view: ViewId,
        fresh: Fresh,
    ) -> Result<PreparedRepositoryMutation> {
        tracing::debug_span!("resolve_fresh_content", count = fresh.content.len()).in_scope(
            || -> Result<()> {
                for id in &fresh.content {
                    self.content
                        .resolve(*id)
                        .map_err(RepositoryPublicationError::Content)?;
                }
                Ok(())
            },
        )?;
        let retained = RetainedSetStore::new(self.objects);
        let retained_changes = retained
            .update_batch(
                old.retained_changes,
                fresh.changes.iter().map(|id| ((*id).into(), true)),
            )
            .await?;
        let retained_views = retained
            .update_batch(
                old.retained_views,
                fresh.views.iter().map(|id| ((*id).into(), true)),
            )
            .await?;
        let retained_revisions = retained
            .update_batch(
                old.retained_revisions,
                fresh.revisions.iter().map(|id| ((*id).into(), true)),
            )
            .await?;
        let retained_applications = retained
            .update_batch(
                old.retained_applications,
                fresh.applications.iter().map(|id| ((*id).into(), true)),
            )
            .await?;
        let required_content = ContentSetStore::new(self.objects)
            .update_batch(
                old.required_content,
                fresh.content.iter().map(|id| (*id, true)),
            )
            .await?;
        let view_lineage = crate::ViewLineageMapStore::new(self.objects)
            .insert_edges(old.view_lineage, fresh.view_edges.iter().copied())
            .await?;
        let state = RepositoryStateRecord {
            base_view: old.base_view,
            changes: old.changes,
            view_lineage,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
        };
        self.objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await?;
        Ok(PreparedRepositoryMutation {
            expected,
            state: state.id(),
            resulting_view,
            objects: fresh.objects + 1,
            content: fresh.content.len(),
            _sealed: (),
        })
    }
}

fn validate_initial(edits: &RevisionEdits) -> Result<()> {
    edits
        .validate()
        .map_err(|_| RepositoryError::InvalidEditTransition("invalid local edit selection"))?;
    if !edits.transition.preserved.is_empty()
        || !edits.transition.removed.is_empty()
        || !edits.transition.subsumed.is_empty()
        || !edits.transition.resolutions.is_empty()
        || edits.transition.added != edits.edits.keys().copied().collect()
    {
        return Err(RepositoryError::InvalidEditTransition(
            "initial revision has predecessor lineage",
        )
        .into());
    }
    Ok(())
}

fn path_components(path: &str) -> Vec<String> {
    path.split('/').map(str::to_owned).collect()
}

/// The distinct paths `edits` touch, sorted so one tree traversal resolves them all.
fn edit_paths(edits: &BTreeMap<EditId, EditRecord>) -> Vec<&str> {
    edits
        .values()
        .map(|edit| edit.path.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn byte_range(range: std::ops::Range<usize>) -> ByteRange {
    ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}

impl Active {
    fn rebuild_graph(&mut self) {
        self.reverse_dependencies.clear();
        for change in self.revisions.keys() {
            self.reverse_dependencies.entry(*change).or_default();
        }
        for (child, (_, revision)) in &self.revisions {
            for dependency in &revision.dependencies {
                self.reverse_dependencies
                    .entry(*dependency)
                    .or_default()
                    .insert(*child);
            }
        }
    }
}

/// The number of leading changes in `order` that the persisted composition already applied with
/// the same revision at the same position. Pending changes are never part of it.
fn unchanged_prefix(
    active: &Active,
    order: &[ChangeId],
    pending: &BTreeMap<ChangeId, PendingRevision>,
) -> usize {
    order
        .iter()
        .zip(&active.persisted_order)
        .take_while(|(new, old)| {
            new == old
                && !pending.contains_key(new)
                && active.persisted_revisions.get(new)
                    == active.revisions.get(new).map(|(revision, _)| revision)
        })
        .count()
}

/// The generation of a revision whose parents have `parents`' generations.
pub(crate) fn generation_after(parents: impl IntoIterator<Item = u64>) -> Result<u64> {
    Ok(
        next_generation(parents).ok_or(RepositoryError::InvalidRepositoryState(
            "revision generation overflow",
        ))?,
    )
}

fn topological(
    records: &BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    selected: &BTreeSet<ChangeId>,
) -> Result<Vec<ChangeId>> {
    topological_by(selected, |id| {
        records.get(id).map(|(_, record)| &record.dependencies)
    })
}

/// Canonical order of `selected`: dependencies first, ties broken by the lowest change id.
pub(crate) fn topological_by<'a>(
    selected: &BTreeSet<ChangeId>,
    dependencies_of: impl Fn(&ChangeId) -> Option<&'a BTreeSet<ChangeId>>,
) -> Result<Vec<ChangeId>> {
    let mut remaining = BTreeMap::new();
    let mut reverse: BTreeMap<ChangeId, Vec<ChangeId>> = BTreeMap::new();
    for id in selected {
        let Some(dependencies) = dependencies_of(id) else {
            return Err(RepositoryError::UnknownDependency(*id).into());
        };
        remaining.insert(*id, dependencies.intersection(selected).count());
        for dependency in dependencies.intersection(selected) {
            reverse.entry(*dependency).or_default().push(*id);
        }
    }
    let mut ready = remaining
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(selected.len());
    while let Some(id) = ready.pop_first() {
        ordered.push(id);
        if let Some(children) = reverse.get(&id) {
            for child in children {
                let count = remaining.get_mut(child).expect("selected child");
                *count -= 1;
                if *count == 0 {
                    ready.insert(*child);
                }
            }
        }
    }
    if ordered.len() != selected.len() {
        return Err(RepositoryError::InvalidRepositoryState("dependency cycle").into());
    }
    Ok(ordered)
}

fn descendants(
    root: ChangeId,
    reverse: &BTreeMap<ChangeId, BTreeSet<ChangeId>>,
) -> BTreeSet<ChangeId> {
    let mut found = BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if let Some(children) = reverse.get(&id) {
            for child in children {
                if found.insert(*child) {
                    pending.push(*child);
                }
            }
        }
    }
    found
}
