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
    target: MutationTarget,
}

/// Which repository view a persisted mutation advances.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MutationTarget {
    /// Advance only the mutable working view, leaving the published view unchanged.
    #[default]
    WorkingOnly,
    /// Advance the working and published views to the mutation's resulting view.
    PublishWorking,
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

struct Active {
    state: RepositoryStateRecord,
    base: ViewRecord,
    working: ViewRecord,
    revisions: BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    /// The revision identities and canonical order represented by `working` before this mutation.
    persisted_revisions: BTreeMap<ChangeId, RevisionId>,
    persisted_order: Vec<ChangeId>,
    reverse_dependencies: BTreeMap<ChangeId, BTreeSet<ChangeId>>,
}

#[derive(Default)]
struct Fresh {
    changes: BTreeSet<ChangeObjectId>,
    views: BTreeSet<ViewId>,
    revisions: BTreeSet<RevisionId>,
    applications: BTreeSet<ApplicationId>,
    content: BTreeSet<ContentId>,
    objects: usize,
}

impl<'a, O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'a, O, P> {
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore, publications: &'a P) -> Self {
        Self {
            objects,
            content,
            publications,
            target: MutationTarget::default(),
        }
    }

    /// Configure whether successful mutations also publish their resulting working view.
    #[must_use]
    pub fn with_target(mut self, target: MutationTarget) -> Self {
        self.target = target;
        self
    }

    pub async fn create(
        &self,
        expected: RepositoryHead,
        proposal: ChangeProposal,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected).await?;
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
        mut proposal: ChangeProposal,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected).await?;
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
        authored_against_working: bool,
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
        let target = if authored_against_working {
            active.working
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
        let ApplicationResultRecord::Clean { tree } = preliminary.result else {
            return Err(RepositoryError::InvalidEdits.into());
        };
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
        let working = self.compose(&active, &selected, &mut fresh).await?;
        let outcome = RevisionOutcome {
            revision_id,
            resulting_view: working.id(),
            reapplied: Vec::new(),
        };
        let prepared = self
            .finish(expected, active.state, working.id(), fresh)
            .await?;
        Ok((prepared, outcome))
    }

    pub async fn resolve_conflict(
        &self,
        expected: RepositoryHead,
        resolution: PersistedConflictResolution,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let active = self.open(expected).await?;
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
            .get(active.working.applications, change)
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
        for (path, after_value) in &resolution.resolved {
            let components = path_components(path);
            let partial_value = trees.lookup(partial_tree, &components).await?;
            if partial_value == *after_value {
                continue;
            }
            if !conflict_paths.contains(path.as_str()) {
                return Err(RepositoryError::InvalidConflictResolution.into());
            }
            let before_value = trees.lookup(target.tree, &components).await?;
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
                self.content
                    .store_generated(id, &bytes)
                    .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
            }
            replacements.extend(captured.edits);
        }

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
        self.revise_at(
            expected,
            change,
            resolution.conflict_revision,
            RevisionMetadata::default(),
            revision_edits,
            Some(application.target_view),
        )
        .await
    }

    pub async fn revise(
        &self,
        expected: RepositoryHead,
        change: ChangeId,
        predecessor: RevisionId,
        metadata: RevisionMetadata,
        edits: RevisionEdits,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        self.revise_at(expected, change, predecessor, metadata, edits, None)
            .await
    }

    async fn revise_at(
        &self,
        expected: RepositoryHead,
        change: ChangeId,
        predecessor: RevisionId,
        metadata: RevisionMetadata,
        edits: RevisionEdits,
        target_override: Option<ViewId>,
    ) -> Result<(PreparedRepositoryMutation, RevisionOutcome)> {
        let mut active = self.open(expected).await?;
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
        // A revision is authored against exactly the predecessor's authenticated target.  Loading
        // that view is both cheaper and more precise than reconstructing its dependency closure.
        let target = RepositoryReader::new(self.objects, self.content)
            .view(target_override.unwrap_or(old.authored_against))
            .await?;
        let preliminary = self
            .apply(
                RevisionId::from_digest([0; 32]),
                &target,
                &edits.edits,
                &mut fresh,
                false,
            )
            .await?;
        let ApplicationResultRecord::Clean { tree } = preliminary.result else {
            return Err(RepositoryError::InvalidEdits.into());
        };
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
        let working = self.compose(&active, &selected, &mut fresh).await?;
        let outcome = RevisionOutcome {
            revision_id,
            resulting_view: working.id(),
            reapplied: topological(&active.revisions, &descendants)?
                .into_iter()
                .filter(|id| *id != change)
                .collect(),
        };
        let prepared = self
            .finish(expected, active.state, working.id(), fresh)
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
            .prepare_envelope_unchecked(prepared.state, old.branches)
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

    async fn open(&self, expected: RepositoryHead) -> Result<Active> {
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
        let base = reader.view(state.base_view).await?;
        let working = reader.view(state.working_view).await?;
        let mut revisions = BTreeMap::new();
        let mut cursor = RevisionMapStore::new(self.objects)
            .cursor(working.revisions)
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
            working,
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

    async fn compose(
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
            active.working
        } else {
            let next = active.persisted_order[prefix];
            let application = applications
                .get(active.working.applications, next)
                .await?
                .ok_or(RepositoryError::InvalidRepositoryState(
                    "working view lacks canonical application",
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
            let application = self.apply(*revision_id, &view, &edits, fresh, true).await?;
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
            view = ViewRecord {
                tree,
                revisions: revision_root,
                applications: application_root,
                unresolved_conflicts: conflict_root,
                required_content: content,
            };
            self.put_view(&view, fresh).await?;
        }
        Ok(view)
    }

    async fn load_edits(&self, revision: &RevisionRecord) -> Result<BTreeMap<EditId, EditRecord>> {
        let reader = RepositoryReader::new(self.objects, self.content);
        let mut edits = BTreeMap::new();
        for id in &revision.edits {
            edits.insert(*id, reader.edit(*id).await?);
        }
        Ok(edits)
    }

    async fn apply(
        &self,
        revision: RevisionId,
        target: &ViewRecord,
        edits: &BTreeMap<EditId, EditRecord>,
        fresh: &mut Fresh,
        persist: bool,
    ) -> Result<ApplicationRecord> {
        let trees = MerkleTreeStore::new(self.objects);
        let mut flat = Tree::new();
        for edit in edits.values() {
            let path = path_components(&edit.path);
            if let Some(value) = trees.lookup(target.tree, &path).await? {
                flat.insert(edit.path.clone(), value);
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
        for (id, bytes) in &output.generated_content {
            self.content
                .store_generated(*id, bytes)
                .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
            fresh.content.insert(*id);
        }
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
        let result = if failed.is_empty() {
            ApplicationResultRecord::Clean { tree }
        } else {
            let conflict = Conflict::new(target.id(), failed);
            if persist {
                self.objects
                    .put_if_absent(conflict.id.into(), &conflict.canonical_bytes())
                    .await?;
                fresh.objects += 1;
            }
            ApplicationResultRecord::Conflicted {
                partial_tree: tree,
                conflict: conflict.id,
            }
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
        Ok(application)
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
        let mut touched = Vec::new();
        for edit in edits.values() {
            let path = path_components(&edit.path);
            let before = trees.lookup(old_view.tree, &path).await?;
            let after = trees.lookup(new_tree, &path).await?;
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
    async fn put_revision(
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
                .get(active.working.applications, change)
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
        working: ViewId,
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
        let published_view = match self.target {
            MutationTarget::WorkingOnly => old.published_view,
            MutationTarget::PublishWorking => working,
        };
        let state = RepositoryStateRecord {
            base_view: old.base_view,
            working_view: working,
            published_view,
            changes: old.changes,
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
