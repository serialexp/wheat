use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Application, ApplicationContext, ApplicationId, ApplicationRecord, ApplicationResult,
    AppliedEditEvidence, ByteRange, CanonicalObject, Change, ChangeId, ChangeMetadata,
    ChangeRevision, Conflict, ConflictId, ConflictResolution, ContentIo, DependencyGraph,
    EditFailure, EditId, EditRecord, FailedEditEvidence, ObjectId, RejectedEdit, RepositoryClosure,
    RepositoryState, RepositoryStateId, RepositoryStateRecord, RepositoryView, RevisionEdits,
    RevisionId, RevisionMetadata, RevisionRecord, Tree, ViewId,
};

mod application;
mod reconstruction;

/// Persisted and materialized objects required to reconstruct an executable repository.
#[derive(Clone, Debug)]
pub struct RepositoryParts {
    pub state: RepositoryStateRecord,
    pub changes: Vec<Change>,
    pub views: Vec<RepositoryView>,
    pub revisions: Vec<(RevisionRecord, Tree, BTreeMap<EditId, EditRecord>)>,
    pub applications: Vec<(ApplicationRecord, Tree, Option<Conflict>)>,
    pub view_lineage: BTreeSet<(ViewId, ViewId)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionOutcome {
    pub revision_id: RevisionId,
    pub resulting_view: ViewId,
    pub reapplied: Vec<ChangeId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeProposal {
    pub id: ChangeId,
    pub metadata: ChangeMetadata,
    pub dependencies: BTreeSet<ChangeId>,
    pub revision_metadata: RevisionMetadata,
    pub edits: RevisionEdits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetPolicyResult {
    Allowed,
    Rejected { unresolved_conflicts: usize },
    UnknownView,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RepositoryError {
    #[error("view {0} does not exist")]
    UnknownView(ViewId),
    #[error("change {0} does not exist")]
    UnknownChange(ChangeId),
    #[error("change {0} already exists")]
    DuplicateChange(ChangeId),
    #[error("revision {0} does not exist")]
    UnknownRevision(RevisionId),
    #[error("revision predecessor is not the current revision of change {0}")]
    StaleRevision(ChangeId),
    #[error("dependency {0} does not exist")]
    UnknownDependency(ChangeId),
    #[error("authored edits do not apply cleanly: {}", describe_rejected(.0))]
    InvalidEdits(Vec<RejectedEdit>),
    #[error("revision edit transition is invalid: {0}")]
    InvalidEditTransition(&'static str),
    #[error("edit application failed: {0}")]
    EditApplication(String),
    #[error("content I/O failed: {0}")]
    ContentIo(String),
    #[error("revision {0} is not the current conflicted application")]
    NotConflicted(RevisionId),
    #[error("conflict resolution changed a path outside the conflict")]
    InvalidConflictResolution,
    #[error("repository state violates an identity invariant: {0}")]
    InvalidRepositoryState(&'static str),
}

/// Rejected edits shown in an error message; a large import can reject many, so
/// only the first few are listed.
const SHOWN_REJECTED_EDITS: usize = 5;

fn describe_rejected(rejected: &[RejectedEdit]) -> String {
    let mut description = rejected
        .iter()
        .take(SHOWN_REJECTED_EDITS)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if rejected.len() > SHOWN_REJECTED_EDITS {
        description.push_str(&format!(
            "; and {} more",
            rejected.len() - SHOWN_REJECTED_EDITS
        ));
    }
    description
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    changes: BTreeMap<ChangeId, Change>,
    views: BTreeMap<ViewId, RepositoryView>,
    revisions: BTreeMap<RevisionId, ChangeRevision>,
    applications: BTreeMap<ApplicationId, Application>,
    base_view: ViewId,
    view_lineage: BTreeSet<(ViewId, ViewId)>,
}

#[derive(Clone, Debug)]
struct ActiveSelection {
    revisions: BTreeMap<ChangeId, RevisionId>,
    dependencies: DependencyGraph,
}

impl Repository {
    #[must_use]
    pub fn new(base_tree: Tree) -> Self {
        let view = RepositoryView::new(base_tree, BTreeMap::new());
        let id = view.id;
        let mut views = BTreeMap::new();
        views.insert(id, view);
        Self {
            changes: BTreeMap::new(),
            views,
            revisions: BTreeMap::new(),
            applications: BTreeMap::new(),
            base_view: id,
            view_lineage: BTreeSet::new(),
        }
    }

    fn active_selection(&self, effective: ViewId) -> Result<ActiveSelection, RepositoryError> {
        let view = self
            .views
            .get(&effective)
            .ok_or(RepositoryError::UnknownView(effective))?;
        let mut dependencies = DependencyGraph::default();
        for (change, revision) in &view.frontier {
            let revision =
                self.revisions
                    .get(revision)
                    .ok_or(RepositoryError::InvalidRepositoryState(
                        "view revision reference",
                    ))?;
            dependencies.add_change(*change, revision.dependencies().clone());
        }
        Ok(ActiveSelection {
            revisions: view.frontier.clone(),
            dependencies,
        })
    }

    fn record_view(&mut self, parent: ViewId, view: RepositoryView) -> ViewId {
        let id = view.id;
        self.views.insert(id, view);
        if parent != id {
            self.view_lineage.insert((parent, id));
        }
        id
    }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        if self.changes.iter().any(|(id, change)| *id != change.id) {
            return Err(RepositoryError::InvalidRepositoryState("change identity"));
        }
        if self.views.iter().any(|(id, view)| {
            *id != view.id || view.id != ViewId::of_canonical_bytes(&view.canonical_bytes())
        }) {
            return Err(RepositoryError::InvalidRepositoryState("view identity"));
        }
        if self.revisions.iter().any(|(id, revision)| {
            *id != revision.id
                || revision.id != RevisionId::of_canonical_bytes(&revision.canonical_bytes())
        }) {
            return Err(RepositoryError::InvalidRepositoryState("revision identity"));
        }
        if self.applications.iter().any(|(id, application)| {
            *id != application.id
                || application.id
                    != ApplicationId::of_canonical_bytes(&application.canonical_bytes())
        }) {
            return Err(RepositoryError::InvalidRepositoryState(
                "application identity",
            ));
        }
        for application in self.applications.values() {
            if let ApplicationResult::Conflicted { conflict, .. } = &application.result
                && conflict.id != ConflictId::of_canonical_bytes(&conflict.canonical_bytes())
            {
                return Err(RepositoryError::InvalidRepositoryState("conflict identity"));
            }
        }
        if !self.views.contains_key(&self.base_view) {
            return Err(RepositoryError::InvalidRepositoryState(
                "repository base view root",
            ));
        }
        if self.view_lineage.iter().any(|(parent, child)| {
            parent == child || !self.views.contains_key(parent) || !self.views.contains_key(child)
        }) {
            return Err(RepositoryError::InvalidRepositoryState(
                "repository view lineage",
            ));
        }
        Ok(())
    }

    /// Validates edit lineage against the repository, rather than merely the new selection.
    fn validate_edit_transition(
        &self,
        change: ChangeId,
        predecessor: Option<RevisionId>,
        edits: &RevisionEdits,
    ) -> Result<(), RepositoryError> {
        edits
            .validate()
            .map_err(|_| RepositoryError::InvalidEditTransition("invalid local edit selection"))?;
        let selected = edits.edits.keys().copied().collect::<BTreeSet<_>>();
        let transition = &edits.transition;

        let Some(predecessor_id) = predecessor else {
            if !transition.preserved.is_empty()
                || !transition.removed.is_empty()
                || !transition.subsumed.is_empty()
                || transition.added != selected
                || !transition.resolutions.is_empty()
            {
                return Err(RepositoryError::InvalidEditTransition(
                    "initial revision has predecessor lineage",
                ));
            }
            return Ok(());
        };
        let revision = self
            .revisions
            .get(&predecessor_id)
            .ok_or(RepositoryError::UnknownRevision(predecessor_id))?;
        if revision.change_id != change {
            return Err(RepositoryError::InvalidEditTransition(
                "predecessor belongs to another change",
            ));
        }
        let predecessor_edits = revision.edits();

        let predecessor_selected = predecessor_edits.keys().copied().collect::<BTreeSet<_>>();
        if predecessor_selected
            != transition
                .preserved
                .union(&transition.removed)
                .copied()
                .collect()
        {
            return Err(RepositoryError::InvalidEditTransition(
                "predecessor edit partition",
            ));
        }
        if transition
            .preserved
            .iter()
            .any(|id| predecessor_edits.get(id) != edits.edits.get(id))
        {
            return Err(RepositoryError::InvalidEditTransition(
                "preserved edit definition changed",
            ));
        }

        for evidence in &transition.resolutions {
            let application = self.applications.get(&evidence.application).ok_or(
                RepositoryError::InvalidEditTransition("resolution application is not retained"),
            )?;
            if application.revision_id != predecessor_id
                || self
                    .revisions
                    .get(&application.revision_id)
                    .map(|r| r.change_id)
                    != Some(change)
            {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution application does not match the predecessor",
                ));
            }
            let ApplicationResult::Conflicted { conflict, .. } = &application.result else {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution application is not conflicted",
                ));
            };
            if !evidence
                .conflicted_edits
                .is_subset(&conflict.edits.keys().copied().collect::<BTreeSet<_>>())
            {
                return Err(RepositoryError::InvalidEditTransition(
                    "resolution conflicted edits were not failed",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn canonical_closure(&self) -> RepositoryClosure {
        let mut objects = BTreeMap::<ObjectId, Vec<u8>>::new();
        let mut required_content = BTreeSet::new();
        let mut retained_changes = BTreeSet::new();
        let mut change_objects = BTreeMap::new();

        for change in self.changes.values() {
            let bytes = change.canonical_bytes();
            let id = crate::ChangeObjectId::of_canonical_bytes(&bytes);
            retained_changes.insert(id);
            change_objects.insert(change.id, id);
            insert_canonical_object(&mut objects, id.into(), bytes);
        }
        for revision in self.revisions.values() {
            insert_canonical_object(&mut objects, revision.id.into(), revision.canonical_bytes());
            for (edit_id, edit) in revision.edits() {
                insert_canonical_object(&mut objects, (*edit_id).into(), edit.canonical_bytes());
                match &edit.operation {
                    crate::EditOperation::GuardedText {
                        before,
                        expected,
                        replacement,
                        after,
                        ..
                    } => {
                        required_content.extend([*before, *expected, *replacement, *after]);
                    }
                    crate::EditOperation::ExactFile {
                        expected,
                        replacement,
                    } => {
                        required_content.extend(
                            expected
                                .iter()
                                .chain(replacement)
                                .map(|value| value.content),
                        );
                    }
                }
            }
            insert_tree_object(&mut objects, revision.resulting_tree());
            required_content.extend(
                revision
                    .resulting_tree()
                    .values()
                    .map(|value| value.content),
            );
        }
        for application in self.applications.values() {
            insert_canonical_object(
                &mut objects,
                application.id.into(),
                application.canonical_bytes(),
            );
            insert_tree_object(&mut objects, application.result.materialized_tree());
            required_content.extend(
                application
                    .result
                    .materialized_tree()
                    .values()
                    .map(|value| value.content),
            );
            if let ApplicationResult::Conflicted { conflict, .. } = &application.result {
                insert_canonical_object(
                    &mut objects,
                    conflict.id.into(),
                    conflict.canonical_bytes(),
                );
                required_content.extend(
                    conflict
                        .edits
                        .values()
                        .filter_map(|evidence| evidence.target)
                        .map(|value| value.content),
                );
            }
        }
        for view in self.views.values() {
            insert_tree_object(&mut objects, &view.tree);
            insert_canonical_object(&mut objects, view.id.into(), view.canonical_bytes());
            required_content.extend(view.required_content.iter().copied());
            crate::build_view_record(view, &mut |object: CanonicalObject| {
                insert_canonical_object(&mut objects, object.id, object.bytes);
            });
            let manifest_bytes = crate::wire::content_manifest_bytes(&view.required_content);
            let manifest_id = crate::wire::content_manifest_id(&view.required_content);
            insert_canonical_object(&mut objects, manifest_id.into(), manifest_bytes);
        }
        crate::build_content_manifest_record(&required_content, &mut |object: CanonicalObject| {
            insert_canonical_object(&mut objects, object.id, object.bytes);
        });
        let aggregate_manifest_bytes = crate::wire::content_manifest_bytes(&required_content);
        let aggregate_manifest_id = crate::wire::content_manifest_id(&required_content);
        insert_canonical_object(
            &mut objects,
            aggregate_manifest_id.into(),
            aggregate_manifest_bytes,
        );

        let view_lineage = crate::build_view_lineage_map(
            self.view_lineage.iter().copied(),
            &mut |object: CanonicalObject| {
                insert_canonical_object(&mut objects, object.id, object.bytes);
            },
        );
        let mut state = RepositoryState {
            id: RepositoryStateId::from_digest([0; 32]),
            base_view: self.base_view,
            change_objects,
            view_lineage,
            required_content: aggregate_manifest_id,
            retained_changes,
            retained_views: self.views.keys().copied().collect(),
            retained_revisions: self.revisions.keys().copied().collect(),
            retained_applications: self.applications.keys().copied().collect(),
        };
        crate::build_repository_state_record(&state, &mut |object: CanonicalObject| {
            insert_canonical_object(&mut objects, object.id, object.bytes);
        });
        let state_bytes = state.canonical_bytes();
        state.id = RepositoryStateId::of_canonical_bytes(&state_bytes);
        insert_canonical_object(&mut objects, state.id.into(), state_bytes);

        RepositoryClosure {
            state,
            objects: objects
                .into_iter()
                .map(|(id, bytes)| CanonicalObject { id, bytes })
                .collect(),
            required_content,
        }
    }

    #[must_use]
    pub fn base_view(&self) -> &RepositoryView {
        &self.views[&self.base_view]
    }

    #[must_use]
    pub fn view(&self, view: ViewId) -> Option<&RepositoryView> {
        self.views.get(&view)
    }

    /// Iterate the complete retained view authority in canonical ID order.
    pub fn retained_views(&self) -> impl Iterator<Item = &RepositoryView> {
        self.views.values()
    }

    /// Iterate authenticated parent-to-child view-lineage edges in canonical order.
    pub fn view_lineage(&self) -> impl Iterator<Item = (ViewId, ViewId)> + '_ {
        self.view_lineage.iter().copied()
    }

    #[must_use]
    pub fn change(&self, change: ChangeId) -> Option<&Change> {
        self.changes.get(&change)
    }

    #[must_use]
    pub fn revision(&self, revision: RevisionId) -> Option<&ChangeRevision> {
        self.revisions.get(&revision)
    }

    #[must_use]
    pub fn current_revision(&self, effective: ViewId, change: ChangeId) -> Option<RevisionId> {
        self.views.get(&effective)?.frontier.get(&change).copied()
    }

    pub fn current_revisions(
        &self,
        effective: ViewId,
    ) -> Result<impl Iterator<Item = (ChangeId, RevisionId)> + '_, RepositoryError> {
        let view = self
            .views
            .get(&effective)
            .ok_or(RepositoryError::UnknownView(effective))?;
        Ok(view
            .frontier
            .iter()
            .map(|(change, revision)| (*change, *revision)))
    }

    pub fn conflicts(
        &self,
        effective: ViewId,
    ) -> Result<impl Iterator<Item = &Application>, RepositoryError> {
        let view = self
            .views
            .get(&effective)
            .ok_or(RepositoryError::UnknownView(effective))?;
        Ok(view
            .applications
            .values()
            .filter_map(|id| self.applications.get(id))
            .filter(|application| {
                matches!(application.result, ApplicationResult::Conflicted { .. })
            }))
    }

    pub fn conflicts_in_view(
        &self,
        view: ViewId,
    ) -> Result<impl Iterator<Item = &Conflict>, RepositoryError> {
        let view = self
            .views
            .get(&view)
            .ok_or(RepositoryError::UnknownView(view))?;
        Ok(view
            .unresolved_conflicts
            .iter()
            .filter_map(|conflict| self.conflict(*conflict)))
    }

    pub fn conflicts_for_change(
        &self,
        view: ViewId,
        change: ChangeId,
    ) -> Result<impl Iterator<Item = &Conflict>, RepositoryError> {
        let view = self
            .views
            .get(&view)
            .ok_or(RepositoryError::UnknownView(view))?;
        let application = view
            .applications
            .get(&change)
            .and_then(|id| self.applications.get(id));
        Ok(application
            .into_iter()
            .filter_map(|application| match &application.result {
                ApplicationResult::Conflicted { conflict, .. } => Some(conflict),
                ApplicationResult::Clean { .. } => None,
            }))
    }

    pub fn conflicts_for_path<'a>(
        &'a self,
        view: ViewId,
        path: &'a str,
    ) -> Result<impl Iterator<Item = &'a Conflict> + 'a, RepositoryError> {
        Ok(self.conflicts_in_view(view)?.filter(move |conflict| {
            conflict.edits.keys().any(|edit| {
                self.revisions.values().any(|revision| {
                    revision
                        .edits()
                        .get(edit)
                        .is_some_and(|record| record.path == path)
                })
            })
        }))
    }

    #[must_use]
    pub fn evaluate_no_unresolved_conflicts(&self, view: ViewId) -> TargetPolicyResult {
        match self.views.get(&view) {
            Some(view) if view.unresolved_conflicts.is_empty() => TargetPolicyResult::Allowed,
            Some(view) => TargetPolicyResult::Rejected {
                unresolved_conflicts: view.unresolved_conflicts.len(),
            },
            None => TargetPolicyResult::UnknownView,
        }
    }

    #[must_use]
    pub fn current_application(&self, effective: ViewId, change: ChangeId) -> Option<&Application> {
        let id = self.views.get(&effective)?.applications.get(&change)?;
        self.applications.get(id)
    }

    pub fn view_for_change<C: ContentIo + ?Sized>(
        &mut self,
        effective: ViewId,
        change: ChangeId,
        context: ApplicationContext<'_, C>,
    ) -> Result<&RepositoryView, RepositoryError> {
        let selection = self.active_selection(effective)?;
        if !selection.revisions.contains_key(&change) {
            return Err(RepositoryError::UnknownChange(change));
        }
        let mut closure = BTreeSet::new();
        Self::collect_ancestors(&selection.dependencies, change, &mut closure)?;
        let view = self.compose(&selection, &closure, context)?;
        Ok(&self.views[&view.id])
    }

    #[must_use]
    pub fn conflicted_application(
        &self,
        effective: ViewId,
        revision: RevisionId,
    ) -> Option<&Application> {
        let change = self.revisions.get(&revision)?.change_id;
        let application = self.current_application(effective, change)?;
        (application.revision_id == revision
            && matches!(application.result, ApplicationResult::Conflicted { .. }))
        .then_some(application)
    }

    #[must_use]
    pub fn application(&self, revision: RevisionId, target: ViewId) -> Option<&Application> {
        self.applications.values().find(|application| {
            application.revision_id == revision && application.target_view == target
        })
    }

    pub fn applications_for_revision(
        &self,
        revision: RevisionId,
    ) -> impl Iterator<Item = &Application> {
        self.applications
            .values()
            .filter(move |application| application.revision_id == revision)
    }

    fn conflict(&self, id: crate::ConflictId) -> Option<&Conflict> {
        self.applications
            .values()
            .find_map(|application| match &application.result {
                ApplicationResult::Conflicted { conflict, .. } if conflict.id == id => {
                    Some(conflict)
                }
                _ => None,
            })
    }

    pub fn apply_revision<C: ContentIo + ?Sized>(
        &self,
        revision: RevisionId,
        target: &RepositoryView,
        context: ApplicationContext<'_, C>,
    ) -> Result<Application, RepositoryError> {
        let revision = self
            .revisions
            .get(&revision)
            .ok_or(RepositoryError::UnknownRevision(revision))?;
        application::execute(revision.id, target, revision.edits(), context, false)
    }

    pub fn create_change<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        dependencies: impl IntoIterator<Item = ChangeId>,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<(ChangeId, RevisionOutcome), RepositoryError> {
        let change = ChangeId::new();
        let outcome = self.create_change_with_metadata(
            basis,
            effective,
            change,
            ChangeMetadata::default(),
            dependencies,
            RevisionMetadata::default(),
            edits,
            context,
        )?;
        Ok((change, outcome))
    }

    pub fn create_change_with_id<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        change: ChangeId,
        dependencies: impl IntoIterator<Item = ChangeId>,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        self.create_change_with_metadata(
            basis,
            effective,
            change,
            ChangeMetadata::default(),
            dependencies,
            RevisionMetadata::default(),
            edits,
            context,
        )
    }

    pub fn create_change_with_metadata<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        change: ChangeId,
        change_metadata: ChangeMetadata,
        dependencies: impl IntoIterator<Item = ChangeId>,
        revision_metadata: RevisionMetadata,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        if !self.views.contains_key(&basis) {
            return Err(RepositoryError::UnknownView(basis));
        }
        let selection = self.active_selection(effective)?;
        if self.changes.contains_key(&change) {
            return Err(RepositoryError::DuplicateChange(change));
        }
        let dependencies = dependencies.into_iter().collect::<BTreeSet<_>>();
        for dependency in &dependencies {
            if !selection.revisions.contains_key(dependency) {
                return Err(RepositoryError::UnknownDependency(*dependency));
            }
        }
        self.validate_edit_transition(change, None, &edits)?;

        let before = self.clone();
        self.changes.insert(
            change,
            Change {
                id: change,
                metadata: change_metadata,
            },
        );
        match self.add_revision(
            effective,
            selection,
            change,
            None,
            dependencies,
            revision_metadata,
            edits,
            context,
        ) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                *self = before;
                Err(error)
            }
        }
    }

    pub fn import_changes<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        changes: impl IntoIterator<Item = ChangeProposal>,
        context: ApplicationContext<'_, C>,
    ) -> Result<BTreeMap<ChangeId, RevisionOutcome>, RepositoryError> {
        if !self.views.contains_key(&basis) {
            return Err(RepositoryError::UnknownView(basis));
        }
        let selection = self.active_selection(effective)?;
        let before = self.clone();
        let result = self.import_changes_inner(effective, selection, changes, context);
        if result.is_err() {
            *self = before;
        }
        result
    }

    pub fn revise_change<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        change: ChangeId,
        predecessor: RevisionId,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        self.revise_change_with_metadata(
            basis,
            effective,
            change,
            predecessor,
            RevisionMetadata::default(),
            edits,
            context,
        )
    }

    pub fn revise_change_with_metadata<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        change: ChangeId,
        predecessor: RevisionId,
        metadata: RevisionMetadata,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        if !self.views.contains_key(&basis) {
            return Err(RepositoryError::UnknownView(basis));
        }
        let selection = self.active_selection(effective)?;
        if selection.revisions.get(&change).copied() != Some(predecessor) {
            return Err(if selection.revisions.contains_key(&change) {
                RepositoryError::StaleRevision(change)
            } else {
                RepositoryError::UnknownChange(change)
            });
        }
        self.validate_edit_transition(change, Some(predecessor), &edits)?;
        let dependencies = selection
            .dependencies
            .parents(change)
            .cloned()
            .ok_or(RepositoryError::UnknownChange(change))?;
        let before = self.clone();
        let result = self.add_revision(
            effective,
            selection,
            change,
            Some(predecessor),
            dependencies,
            metadata,
            edits,
            context,
        );
        if result.is_err() {
            *self = before;
        }
        result
    }

    pub fn resolve_conflict<C: ContentIo + ?Sized>(
        &mut self,
        basis: ViewId,
        effective: ViewId,
        resolution: ConflictResolution,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        if !self.views.contains_key(&basis) {
            return Err(RepositoryError::UnknownView(basis));
        }
        let selection = self.active_selection(effective)?;
        let revision = self.revisions.get(&resolution.conflict_revision).ok_or(
            RepositoryError::UnknownRevision(resolution.conflict_revision),
        )?;
        let change = revision.change_id;
        if selection.revisions.get(&change).copied() != Some(resolution.conflict_revision) {
            return Err(RepositoryError::StaleRevision(change));
        }
        let application = self
            .conflicted_application(effective, resolution.conflict_revision)
            .ok_or(RepositoryError::NotConflicted(resolution.conflict_revision))?;
        let ApplicationResult::Conflicted {
            partial_tree,
            conflict,
        } = &application.result
        else {
            unreachable!()
        };
        let conflict_paths = conflict
            .edits
            .keys()
            .filter_map(|id| revision.edits().get(id))
            .map(|edit| edit.path.as_str())
            .collect::<BTreeSet<_>>();
        let changed_paths = changed_paths(partial_tree, &resolution.resolved_tree);
        if changed_paths
            .iter()
            .any(|path| !conflict_paths.contains(path.as_str()))
        {
            return Err(RepositoryError::InvalidConflictResolution);
        }

        let target = self.views[&application.target_view].clone();
        let mut edits = BTreeMap::new();
        let mut generated = BTreeMap::new();
        for path in changed_paths {
            let before_value = target.tree.get(&path).copied();
            let after_value = resolution.resolved_tree.get(&path).copied();
            let before_bytes = before_value
                .map(|v| {
                    context
                        .content
                        .resolve(v.content)
                        .map_err(|e| RepositoryError::ContentIo(e.to_string()))
                })
                .transpose()?;
            let after_bytes = after_value
                .map(|v| {
                    context
                        .content
                        .resolve(v.content)
                        .map_err(|e| RepositoryError::ContentIo(e.to_string()))
                })
                .transpose()?;
            let before = before_value.zip(before_bytes.as_deref()).map(|(v, bytes)| {
                crate::edit_capture::FileSnapshot {
                    kind: v.kind,
                    bytes,
                }
            });
            let after = after_value.zip(after_bytes.as_deref()).map(|(v, bytes)| {
                crate::edit_capture::FileSnapshot {
                    kind: v.kind,
                    bytes,
                }
            });
            let captured = crate::edit_capture::capture_edit(
                &path,
                before,
                after,
                None,
                context.limits.guarded,
            )
            .map_err(|error| RepositoryError::EditApplication(format!("capture: {error:?}")))?;
            edits.extend(captured.edits);
            generated.extend(captured.content);
        }
        for (id, bytes) in generated {
            context
                .content
                .store_generated(id, &bytes)
                .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
        }
        let replacement_edits = edits.keys().copied().collect::<BTreeSet<_>>();
        let removed = conflict.edits.keys().copied().collect::<BTreeSet<_>>();
        let preserved = revision
            .edits()
            .keys()
            .filter(|id| !removed.contains(id))
            .copied()
            .collect::<BTreeSet<_>>();
        let mut revision_edit_records = revision.edits().clone();
        revision_edit_records.retain(|id, _| preserved.contains(id));
        revision_edit_records.extend(edits);
        let revision_edits = RevisionEdits {
            edits: revision_edit_records,
            transition: crate::EditTransition {
                preserved,
                removed,
                added: replacement_edits.clone(),
                resolutions: vec![crate::ConflictResolutionEvidence {
                    application: application.id,
                    conflicted_edits: conflict.edits.keys().copied().collect(),
                    replacement_edits,
                }],
                ..crate::EditTransition::default()
            },
        };
        let dependencies = selection
            .dependencies
            .parents(change)
            .cloned()
            .ok_or(RepositoryError::UnknownChange(change))?;
        let before = self.clone();
        let result = self.add_revision(
            effective,
            selection,
            change,
            Some(resolution.conflict_revision),
            dependencies,
            RevisionMetadata::default(),
            revision_edits,
            context,
        );
        if result.is_err() {
            *self = before;
        }
        result
    }

    fn import_changes_inner<C: ContentIo + ?Sized>(
        &mut self,
        _effective: ViewId,
        mut selection: ActiveSelection,
        changes: impl IntoIterator<Item = ChangeProposal>,
        context: ApplicationContext<'_, C>,
    ) -> Result<BTreeMap<ChangeId, RevisionOutcome>, RepositoryError> {
        // Validate every initial lineage before an earlier proposal can resolve or sink content.
        let proposals = changes.into_iter().collect::<Vec<_>>();
        for proposal in &proposals {
            self.validate_edit_transition(proposal.id, None, &proposal.edits)?;
        }

        let mut outcomes = BTreeMap::<ChangeId, RevisionOutcome>::new();
        for proposal in proposals {
            if self.changes.contains_key(&proposal.id) {
                return Err(RepositoryError::DuplicateChange(proposal.id));
            }
            for dependency in &proposal.dependencies {
                if !selection.revisions.contains_key(dependency) {
                    return Err(RepositoryError::UnknownDependency(*dependency));
                }
            }
            self.validate_edit_transition(proposal.id, None, &proposal.edits)?;

            let target = if proposal.dependencies.is_empty() {
                self.views[&self.base_view].clone()
            } else if proposal.dependencies.len() == 1 {
                let dependency = proposal.dependencies.first().expect("length checked");
                if let Some(outcome) = outcomes.get(dependency) {
                    self.views[&outcome.resulting_view].clone()
                } else {
                    let mut closure = BTreeSet::new();
                    Self::collect_ancestors(&selection.dependencies, *dependency, &mut closure)?;
                    self.compose(&selection, &closure, context)?
                }
            } else {
                let mut closure = BTreeSet::new();
                for dependency in &proposal.dependencies {
                    Self::collect_ancestors(&selection.dependencies, *dependency, &mut closure)?;
                }
                self.compose(&selection, &closure, context)?
            };
            let pending_id = RevisionId::from_digest([0; 32]);
            let preliminary =
                application::execute(pending_id, &target, &proposal.edits.edits, context, false)?;
            let tree = match &preliminary.result {
                ApplicationResult::Clean { tree } => tree,
                ApplicationResult::Conflicted { conflict, .. } => {
                    return Err(RepositoryError::InvalidEdits(RejectedEdit::collect(
                        &proposal.edits.edits,
                        &conflict.edits,
                    )));
                }
            };
            let mut revision = ChangeRevision {
                id: RevisionId::from_digest([0; 32]),
                change_id: proposal.id,
                metadata: proposal.revision_metadata,
                predecessor: None,
                dependencies: proposal.dependencies.clone(),
                authored_against: target.id,
                edits: proposal.edits,
                resulting_tree: tree.clone(),
            };
            revision.id = RevisionId::of_canonical_bytes(&revision.canonical_bytes());
            let application =
                application::execute(revision.id, &target, revision.edits(), context, true)?;
            let mut frontier = target.frontier.clone();
            frontier.insert(proposal.id, revision.id);
            let mut applications = target.applications.clone();
            applications.insert(proposal.id, application.id);
            let mut conflicts = target.unresolved_conflicts.clone();
            if let ApplicationResult::Conflicted { conflict, .. } = &application.result {
                conflicts.insert(conflict.id);
            }
            let view = RepositoryView::from_manifest(
                application.result.materialized_tree().clone(),
                frontier,
                applications,
                conflicts,
            );

            self.changes.insert(
                proposal.id,
                Change {
                    id: proposal.id,
                    metadata: proposal.metadata,
                },
            );
            selection
                .dependencies
                .add_change(proposal.id, proposal.dependencies);
            selection.revisions.insert(proposal.id, revision.id);
            self.revisions.insert(revision.id, revision.clone());
            self.applications.insert(application.id, application);
            self.record_view(target.id, view.clone());
            outcomes.insert(
                proposal.id,
                RevisionOutcome {
                    revision_id: revision.id,
                    resulting_view: view.id,
                    reapplied: Vec::new(),
                },
            );
        }

        let all = selection.revisions.keys().copied().collect::<BTreeSet<_>>();
        let _resulting = self.compose(&selection, &all, context)?;
        self.validate()?;
        Ok(outcomes)
    }

    fn add_revision<C: ContentIo + ?Sized>(
        &mut self,
        effective: ViewId,
        mut selection: ActiveSelection,
        change: ChangeId,
        predecessor: Option<RevisionId>,
        dependencies: BTreeSet<ChangeId>,
        metadata: RevisionMetadata,
        edits: RevisionEdits,
        context: ApplicationContext<'_, C>,
    ) -> Result<RevisionOutcome, RepositoryError> {
        self.validate_edit_transition(change, predecessor, &edits)?;
        selection
            .dependencies
            .add_change(change, dependencies.clone());
        let target = self.dependency_view(&selection, change, context)?;
        let preliminary = application::execute(
            RevisionId::from_digest([0; 32]),
            &target,
            &edits.edits,
            context,
            false,
        )?;
        let authored_tree = match &preliminary.result {
            ApplicationResult::Clean { tree } => tree,
            ApplicationResult::Conflicted { conflict, .. } => {
                return Err(RepositoryError::InvalidEdits(RejectedEdit::collect(
                    &edits.edits,
                    &conflict.edits,
                )));
            }
        };
        let mut revision = ChangeRevision {
            id: RevisionId::from_digest([0; 32]),
            change_id: change,
            metadata,
            predecessor,
            dependencies,
            authored_against: target.id,
            edits,
            resulting_tree: authored_tree.clone(),
        };
        let revision_id = RevisionId::of_canonical_bytes(&crate::wire::revision_bytes(&revision));
        revision.id = revision_id;
        self.revisions.insert(revision_id, revision);
        selection.revisions.insert(change, revision_id);

        let reapplied = Self::topological_order(
            &selection.dependencies,
            &Self::descendants(&selection.dependencies, change),
        )
        .into_iter()
        .filter(|descendant| *descendant != change)
        .collect::<Vec<_>>();
        let all = selection.revisions.keys().copied().collect::<BTreeSet<_>>();
        let resulting = self.compose(&selection, &all, context)?;
        // Ensure the lineage records a continuation edge from the prior effective view
        // to the new result.  compose rebuilds from base_view and only records intra-
        // compose edges, so a prior effective that was already a descendant of the base
        // would become an orphaned leaf without this edge.
        if effective != self.base_view && effective != resulting.id {
            self.view_lineage.insert((effective, resulting.id));
        }
        Ok(RevisionOutcome {
            revision_id,
            resulting_view: resulting.id,
            reapplied,
        })
    }

    fn dependency_view<C: ContentIo + ?Sized>(
        &mut self,
        selection: &ActiveSelection,
        change: ChangeId,
        context: ApplicationContext<'_, C>,
    ) -> Result<RepositoryView, RepositoryError> {
        let parents = selection
            .dependencies
            .parents(change)
            .ok_or(RepositoryError::UnknownChange(change))?
            .clone();
        let mut closure = BTreeSet::new();
        for parent in parents {
            Self::collect_ancestors(&selection.dependencies, parent, &mut closure)?;
        }
        self.compose(selection, &closure, context)
    }

    fn compose<C: ContentIo + ?Sized>(
        &mut self,
        selection: &ActiveSelection,
        selected: &BTreeSet<ChangeId>,
        context: ApplicationContext<'_, C>,
    ) -> Result<RepositoryView, RepositoryError> {
        let mut target = self.views[&self.base_view].clone();
        for change in Self::topological_order(&selection.dependencies, selected) {
            let revision_id = selection
                .revisions
                .get(&change)
                .copied()
                .ok_or(RepositoryError::UnknownChange(change))?;
            let revision = &self.revisions[&revision_id];
            let application =
                application::execute(revision_id, &target, revision.edits(), context, true)?;
            let mut frontier = target.frontier.clone();
            frontier.insert(change, revision_id);
            let mut applications = target.applications.clone();
            applications.insert(change, application.id);
            let mut conflicts = target.unresolved_conflicts.clone();
            if let ApplicationResult::Conflicted { conflict, .. } = &application.result {
                conflicts.insert(conflict.id);
            }
            let parent = target.id;
            target = RepositoryView::from_manifest(
                application.result.materialized_tree().clone(),
                frontier,
                applications,
                conflicts,
            );
            self.applications.insert(application.id, application);
            self.record_view(parent, target.clone());
        }
        Ok(target)
    }

    fn collect_ancestors(
        dependencies: &DependencyGraph,
        change: ChangeId,
        collected: &mut BTreeSet<ChangeId>,
    ) -> Result<(), RepositoryError> {
        if !collected.insert(change) {
            return Ok(());
        }
        let parents = dependencies
            .parents(change)
            .ok_or(RepositoryError::UnknownChange(change))?;
        for parent in parents {
            Self::collect_ancestors(dependencies, *parent, collected)?;
        }
        Ok(())
    }

    fn descendants(dependencies: &DependencyGraph, root: ChangeId) -> BTreeSet<ChangeId> {
        let mut descendants = BTreeSet::from([root]);
        let mut pending = vec![root];
        while let Some(change) = pending.pop() {
            for child in dependencies.children(change) {
                if descendants.insert(child) {
                    pending.push(child);
                }
            }
        }
        descendants
    }

    fn topological_order(
        dependencies: &DependencyGraph,
        selected: &BTreeSet<ChangeId>,
    ) -> Vec<ChangeId> {
        let mut remaining_parents = selected
            .iter()
            .map(|change| {
                let count = dependencies
                    .parents(*change)
                    .map_or(0, |parents| parents.intersection(selected).count());
                (*change, count)
            })
            .collect::<BTreeMap<_, _>>();
        let mut ready = remaining_parents
            .iter()
            .filter_map(|(change, count)| (*count == 0).then_some(*change))
            .collect::<BTreeSet<_>>();
        let mut ordered = Vec::with_capacity(selected.len());
        while let Some(change) = ready.pop_first() {
            ordered.push(change);
            for child in dependencies.children(change) {
                if let Some(count) = remaining_parents.get_mut(&child) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(child);
                    }
                }
            }
        }
        debug_assert_eq!(ordered.len(), selected.len(), "dependency graph is acyclic");
        ordered
    }
}

fn insert_tree_object(objects: &mut BTreeMap<ObjectId, Vec<u8>>, tree: &Tree) {
    for (id, bytes) in crate::wire::tree_objects(tree) {
        insert_canonical_object(objects, id.into(), bytes);
    }
}

fn insert_canonical_object(
    objects: &mut BTreeMap<ObjectId, Vec<u8>>,
    id: ObjectId,
    bytes: Vec<u8>,
) {
    if let Some(existing) = objects.insert(id, bytes.clone()) {
        assert_eq!(
            existing, bytes,
            "one object ID must name exactly one byte string"
        );
    }
}

fn changed_paths(before: &Tree, after: &Tree) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}
