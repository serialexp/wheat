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
    objects: &'a O,
    content: &'a ContentStore,
    publications: &'a P,
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

pub(crate) struct Active {
    pub(crate) state: RepositoryStateRecord,
    pub(crate) base: ViewRecord,
    pub(crate) effective: ViewRecord,
    pub(crate) revisions: BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    /// The revision identities and canonical order represented by `effective` before this mutation.
    persisted_revisions: BTreeMap<ChangeId, RevisionId>,
    persisted_order: Vec<ChangeId>,
    reverse_dependencies: BTreeMap<ChangeId, BTreeSet<ChangeId>>,
}

/// An application of a revision's edits, with the evidence for each edit that failed
/// (the recorded conflict's edits; empty when the application is clean).
pub(crate) struct AppliedEdits {
    pub(crate) application: ApplicationRecord,
    pub(crate) failed: BTreeMap<EditId, FailedEditEvidence>,
}

impl AppliedEdits {
    /// The clean resulting tree, or [`RepositoryError::InvalidEdits`] naming each edit
    /// that failed and why.
    fn clean_tree(&self, edits: &BTreeMap<EditId, EditRecord>) -> Result<TreeId> {
        match self.application.result {
            ApplicationResultRecord::Clean { tree } => Ok(tree),
            ApplicationResultRecord::Conflicted { .. } => Err(RepositoryError::InvalidEdits(
                RejectedEdit::collect(edits, &self.failed),
            )
            .into()),
        }
    }
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
        self.create_opened(expected, active, proposal, false).await
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
        self.create_opened(expected, active, proposal, true).await
    }

    async fn create_opened(
        &self,
        expected: RepositoryHead,
        mut active: Active,
        proposal: ChangeProposal,
        authored_against_effective: bool,
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
        let target = if authored_against_effective {
            active.effective
        } else {
            self.compose(&active, &proposal.dependencies, &mut fresh)
                .await?
        };
        let preliminary = self
            .apply(
                RevisionId::from_digest([0; 32]),
                &target,
                &proposal.edits.edits,
                &mut fresh,
                false,
            )
            .await?;
        let tree = preliminary.clean_tree(&proposal.edits.edits)?;
        let revision = RevisionRecord {
            change_id: proposal.id,
            metadata: proposal.revision_metadata,
            predecessor: None,
            dependencies: proposal.dependencies,
            authored_against: target.id(),
            edits: proposal.edits.edits.keys().copied().collect(),
            transition: proposal.edits.transition,
            resulting_tree: tree,
        };
        self.persist_edits(&proposal.edits.edits, &mut fresh)
            .await?;
        let revision_id = self.put_revision(&revision, &mut fresh).await?;
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
        active
            .revisions
            .insert(proposal.id, (revision_id, revision));
        active.rebuild_graph();
        let selected = active.revisions.keys().copied().collect();
        let resulting = self.compose(&active, &selected, &mut fresh).await?;
        // Ensure the lineage records a continuation edge from the previous effective
        // view to the new result.  `compose` rebuilds the composed view from the base
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
                .zip(before_bytes.as_deref())
                .map(|(value, bytes)| wheat_core::edit_capture::FileSnapshot {
                    kind: value.kind,
                    bytes,
                });
            let after = after_value
                .zip(after_bytes.as_deref())
                .map(|(value, bytes)| wheat_core::edit_capture::FileSnapshot {
                    kind: value.kind,
                    bytes,
                });
            let captured = wheat_core::edit_capture::capture_edit(
                path,
                before,
                after,
                None,
                wheat_core::guarded::Limits::default(),
            )
            .map_err(|error| RepositoryError::EditApplication(format!("capture: {error:?}")))?;
            for (id, bytes) in captured.content {
                let actual = captured_content.put(&bytes).map_err(content_error)?;
                if actual != id {
                    return Err(RepositoryError::ContentIo(format!(
                        "captured content {id} hashed to {actual}"
                    ))
                    .into());
                }
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
        let mut active = self.open(expected, basis_view, effective_view).await?;
        let Some((current, old)) = active.revisions.get(&change).cloned() else {
            return Err(RepositoryError::UnknownChange(change).into());
        };
        if current != predecessor {
            return Err(RepositoryError::StaleRevision(change).into());
        }
        let old_edits = self.load_edits(&old).await?;
        self.validate_revision_transition(change, predecessor, old_edits, &edits, &active)
            .await?;
        let mut fresh = Fresh::default();
        // A revision is authored against the exact view its predecessor is currently applied to.
        // That equals the predecessor's own authored base until an ancestor revision reapplies
        // the change onto the ancestor's new result; from then on the workspace shows the
        // reapplied result, so a new revision must replay against the reapplied target to
        // reproduce it. Loading the recorded target is also cheaper than recomposing the
        // dependency closure.
        let reader = RepositoryReader::new(self.objects, self.content);
        let application = ApplicationMapStore::new(self.objects)
            .get(active.effective.applications, change)
            .await?
            .ok_or(RepositoryPublicationError::InvalidReferences(
                "active change has no current application",
            ))?;
        let application = reader.application(application).await?;
        if application.revision_id != predecessor {
            return Err(RepositoryPublicationError::InvalidReferences(
                "current application does not correspond to the predecessor revision",
            )
            .into());
        }
        let target = reader.view(application.target_view).await?;
        let preliminary = self
            .apply(
                RevisionId::from_digest([0; 32]),
                &target,
                &edits.edits,
                &mut fresh,
                false,
            )
            .await?;
        let tree = preliminary.clean_tree(&edits.edits)?;
        let revision = RevisionRecord {
            change_id: change,
            metadata,
            predecessor: Some(predecessor),
            dependencies: old.dependencies,
            authored_against: target.id(),
            edits: edits.edits.keys().copied().collect(),
            transition: edits.transition,
            resulting_tree: tree,
        };
        self.persist_edits(&edits.edits, &mut fresh).await?;
        let revision_id = self.put_revision(&revision, &mut fresh).await?;
        active.revisions.insert(change, (revision_id, revision));
        active.rebuild_graph();
        let descendants = descendants(change, &active.reverse_dependencies);
        let selected = active.revisions.keys().copied().collect();
        let resulting = self.compose(&active, &selected, &mut fresh).await?;
        Self::ensure_continuation_edge(&active, &resulting, &mut fresh);
        let outcome = RevisionOutcome {
            revision_id,
            resulting_view: resulting.id(),
            reapplied: topological(&active.revisions, &descendants)?
                .into_iter()
                .filter(|id| *id != change)
                .collect(),
        };
        let prepared = self
            .finish(expected, active.state, resulting.id(), fresh)
            .await?;
        Ok((prepared, outcome))
    }

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
        if effective_view != basis_view
            && !crate::ViewLineageMapStore::new(self.objects)
                .is_descendant(
                    state.view_lineage,
                    basis_view,
                    effective_view,
                    crate::ViewLineageLimits::default(),
                )
                .await?
        {
            return Err(RepositoryPublicationError::InvalidReferences(
                "effective view does not descend from basis view",
            )
            .into());
        }
        let base = reader.view(basis_view).await?;
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
            base,
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
        Ok(active)
    }

    pub(crate) async fn compose(
        &self,
        active: &Active,
        selected: &BTreeSet<ChangeId>,
        fresh: &mut Fresh,
    ) -> Result<ViewRecord> {
        let selected = dependency_closure(selected, &active.revisions)?;
        let order = topological(&active.revisions, &selected)?;
        let revisions = RevisionMapStore::new(self.objects);
        let applications = ApplicationMapStore::new(self.objects);
        let conflicts = ConflictSetStore::new(self.objects);

        // Preserve the exact authenticated identity of the longest unchanged canonical prefix.
        // The target of the next persisted application is precisely that intermediate view, so no
        // edit or tree in the prefix needs to be read.  Boundaries avoid needless map lookups.
        let unchanged_prefix = order
            .iter()
            .zip(&active.persisted_order)
            .take_while(|(new, old)| {
                new == old
                    && active.persisted_revisions.get(new)
                        == active.revisions.get(new).map(|(revision, _)| revision)
            })
            .count();
        let composing_all = selected.len() == active.revisions.len();
        let prefix = if composing_all
            || (unchanged_prefix == order.len() && order.len() <= active.persisted_order.len())
        {
            unchanged_prefix
        } else {
            0
        };
        let mut view = if prefix == 0 {
            active.base
        } else if prefix == active.persisted_order.len() {
            active.effective
        } else {
            let next = active.persisted_order[prefix];
            let application = applications
                .get(active.effective.applications, next)
                .await?
                .ok_or(RepositoryError::InvalidRepositoryState(
                    "effective view lacks canonical application",
                ))?;
            let application = RepositoryReader::new(self.objects, self.content)
                .application(application)
                .await?;
            RepositoryReader::new(self.objects, self.content)
                .view(application.target_view)
                .await?
        };
        for change in order.into_iter().skip(prefix) {
            let (revision_id, revision) = &active.revisions[&change];
            let edits = self.load_edits(revision).await?;
            let application = self
                .apply(*revision_id, &view, &edits, fresh, true)
                .await?
                .application;
            let application_id = application.id();
            let tree = match application.result {
                ApplicationResultRecord::Clean { tree } => tree,
                ApplicationResultRecord::Conflicted { partial_tree, .. } => partial_tree,
            };
            let revision_root = revisions
                .insert(view.revisions, change, *revision_id)
                .await?;
            let application_root = applications
                .insert(view.applications, change, application_id)
                .await?;
            let conflict_root = match application.result {
                ApplicationResultRecord::Conflicted { conflict, .. } => {
                    conflicts
                        .insert(view.unresolved_conflicts, conflict)
                        .await?
                }
                ApplicationResultRecord::Clean { .. } => view.unresolved_conflicts,
            };
            let content = self
                .update_visible_manifest(view.required_content, &edits, tree, &view, fresh)
                .await?;
            let parent = view.id();
            view = ViewRecord {
                tree,
                revisions: revision_root,
                applications: application_root,
                unresolved_conflicts: conflict_root,
                required_content: content,
            };
            self.put_view(&view, fresh).await?;
            if parent != view.id() {
                fresh.view_edges.insert((parent, view.id()));
            }
        }
        Ok(view)
    }

    /// Record a lineage edge from the prior effective view to the new composed result so that the
    /// prior effective view is no longer a leaf frontier.
    ///
    /// `compose` is a pure reconstructor: it rebuilds the view from `active.base` and only records
    /// edges between consecutively-composed intermediate views within a single call.  When the
    /// effective view was already a descendant of the base (from a prior mutation), `compose`
    /// produces a new result branching from the base, leaving the old effective as an orphaned leaf.
    /// This explicit continuation edge connects the two, maintaining a single-frontier chain.
    fn ensure_continuation_edge(active: &Active, resulting: &ViewRecord, fresh: &mut Fresh) {
        let effective_id = active.effective.id();
        let resulting_id = resulting.id();
        // Only needed when the effective view is a non-trivial descendant of the base (otherwise
        // compose's own base→result edge is sufficient) and the result differs from the effective.
        if effective_id != active.base.id() && effective_id != resulting_id {
            fresh.view_edges.insert((effective_id, resulting_id));
        }
    }

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

    pub(crate) async fn apply(
        &self,
        revision: RevisionId,
        target: &ViewRecord,
        edits: &BTreeMap<EditId, EditRecord>,
        fresh: &mut Fresh,
        persist: bool,
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
            allow_already_satisfied: persist,
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
        for (id, bytes) in &output.generated_content {
            let actual = generated.put(bytes).map_err(content_error)?;
            if actual != *id {
                return Err(RepositoryError::ContentIo(format!(
                    "generated content {id} hashed to {actual}"
                ))
                .into());
            }
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
        let tree = trees.apply_edits(target.tree, &tree_edits).await?;
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
            if persist {
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
        let application = ApplicationRecord {
            revision_id: revision,
            target_view: target.id(),
            result,
            applied,
        };
        if persist {
            self.objects
                .put_if_absent(application.id().into(), &application.canonical_bytes())
                .await?;
            fresh.objects += 1;
            fresh.applications.insert(application.id());
        }
        Ok(AppliedEdits {
            application,
            failed,
        })
    }

    async fn update_visible_manifest(
        &self,
        old: ContentManifestId,
        edits: &BTreeMap<EditId, EditRecord>,
        new_tree: TreeId,
        old_view: &ViewRecord,
        fresh: &mut Fresh,
    ) -> Result<ContentManifestId> {
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
        // A content identity may remain visible at another path. The shared updater
        // authenticates the resulting tree before removing such a candidate instead
        // of treating one touched path as a global reference count.
        let root =
            crate::commit_content::update_content(self.objects, old, new_tree, touched).await?;
        let manifest = ContentManifestRecord { content: root };
        self.objects
            .put_if_absent(manifest.id().into(), &manifest.canonical_bytes())
            .await?;
        fresh.objects += 1;
        Ok(manifest.id())
    }

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
    async fn persist_edits(
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

    async fn finish(
        &self,
        expected: RepositoryHead,
        old: RepositoryStateRecord,
        resulting_view: ViewId,
        fresh: Fresh,
    ) -> Result<PreparedRepositoryMutation> {
        for id in &fresh.content {
            self.content
                .resolve(*id)
                .map_err(RepositoryPublicationError::Content)?;
        }
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
        let sets = ContentSetStore::new(self.objects);
        let old_manifest = RepositoryReader::new(self.objects, self.content)
            .manifest(old.required_content)
            .await?;
        let content = sets
            .update_batch(
                old_manifest.content,
                fresh.content.iter().map(|id| (*id, true)),
            )
            .await?;
        let manifest = ContentManifestRecord { content };
        self.objects
            .put_if_absent(manifest.id().into(), &manifest.canonical_bytes())
            .await?;
        let view_lineage = crate::ViewLineageMapStore::new(self.objects)
            .insert_edges(old.view_lineage, fresh.view_edges.iter().copied())
            .await?;
        let state = RepositoryStateRecord {
            base_view: old.base_view,
            changes: old.changes,
            view_lineage,
            required_content: manifest.id(),
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
            objects: fresh.objects + 2,
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

fn dependency_closure(
    roots: &BTreeSet<ChangeId>,
    records: &BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
) -> Result<BTreeSet<ChangeId>> {
    let mut closure = BTreeSet::new();
    let mut pending = roots.iter().copied().collect::<Vec<_>>();
    while let Some(change) = pending.pop() {
        if !closure.insert(change) {
            continue;
        }
        let Some((_, revision)) = records.get(&change) else {
            return Err(RepositoryError::UnknownDependency(change).into());
        };
        pending.extend(revision.dependencies.iter().copied());
    }
    Ok(closure)
}

fn topological(
    records: &BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    selected: &BTreeSet<ChangeId>,
) -> Result<Vec<ChangeId>> {
    let mut remaining = BTreeMap::new();
    let mut reverse: BTreeMap<ChangeId, Vec<ChangeId>> = BTreeMap::new();
    for id in selected {
        let Some((_, record)) = records.get(id) else {
            return Err(RepositoryError::UnknownDependency(*id).into());
        };
        remaining.insert(*id, record.dependencies.intersection(selected).count());
        for dependency in record.dependencies.intersection(selected) {
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
