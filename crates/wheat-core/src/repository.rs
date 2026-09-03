use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Application, ApplicationId, ApplicationResult, ChangeId, ChangeRevision, Conflict,
    ConflictResolution, DependencyGraph, PathConflict, RepositoryView, RevisionId, Transformation,
    Tree, ViewId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionOutcome {
    pub revision_id: RevisionId,
    pub resulting_view: ViewId,
    pub reapplied: Vec<ChangeId>,
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
    #[error("revision {0} does not exist")]
    UnknownRevision(RevisionId),
    #[error("revision predecessor is not the current revision of change {0}")]
    StaleRevision(ChangeId),
    #[error("dependency {0} does not exist")]
    UnknownDependency(ChangeId),
    #[error("authored transformation does not apply cleanly")]
    InvalidTransformation,
    #[error("revision {0} is not the current conflicted application")]
    NotConflicted(RevisionId),
    #[error("conflict resolution changed a path outside the conflict")]
    InvalidConflictResolution,
    #[error("publication expected {expected}, but current view is {actual}")]
    StalePublication { expected: ViewId, actual: ViewId },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    views: BTreeMap<ViewId, RepositoryView>,
    revisions: BTreeMap<RevisionId, ChangeRevision>,
    current_revisions: BTreeMap<ChangeId, RevisionId>,
    dependencies: DependencyGraph,
    applications: BTreeMap<ApplicationId, Application>,
    current_applications: BTreeMap<ChangeId, Application>,
    base_view: ViewId,
    working_view: ViewId,
    published_view: ViewId,
}

impl Repository {
    #[must_use]
    pub fn new(base_tree: Tree) -> Self {
        let view = RepositoryView::new(base_tree, BTreeMap::new());
        let id = view.id;
        let mut views = BTreeMap::new();
        views.insert(id, view);
        Self {
            views,
            revisions: BTreeMap::new(),
            current_revisions: BTreeMap::new(),
            dependencies: DependencyGraph::default(),
            applications: BTreeMap::new(),
            current_applications: BTreeMap::new(),
            base_view: id,
            working_view: id,
            published_view: id,
        }
    }

    #[must_use]
    pub fn working_view(&self) -> &RepositoryView {
        &self.views[&self.working_view]
    }

    #[must_use]
    pub fn published_view(&self) -> &RepositoryView {
        &self.views[&self.published_view]
    }

    #[must_use]
    pub fn view(&self, view: ViewId) -> Option<&RepositoryView> {
        self.views.get(&view)
    }

    #[must_use]
    pub fn revision(&self, revision: RevisionId) -> Option<&ChangeRevision> {
        self.revisions.get(&revision)
    }

    #[must_use]
    pub fn current_revision(&self, change: ChangeId) -> Option<RevisionId> {
        self.current_revisions.get(&change).copied()
    }

    pub fn current_revisions(&self) -> impl Iterator<Item = (ChangeId, RevisionId)> + '_ {
        self.current_revisions
            .iter()
            .map(|(change, revision)| (*change, *revision))
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &Application> {
        self.current_applications.values().filter(|application| {
            matches!(application.result, ApplicationResult::Conflicted { .. })
        })
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
            conflict
                .paths
                .iter()
                .any(|candidate| candidate.path == path)
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
    pub fn current_application(&self, change: ChangeId) -> Option<&Application> {
        self.current_applications.get(&change)
    }

    #[must_use]
    pub fn conflicted_application(&self, revision: RevisionId) -> Option<&Application> {
        let change = self.revisions.get(&revision)?.change_id;
        let application = self.current_applications.get(&change)?;
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

    pub fn apply_revision(
        &self,
        revision: RevisionId,
        target: &RepositoryView,
    ) -> Result<Application, RepositoryError> {
        let revision = self
            .revisions
            .get(&revision)
            .ok_or(RepositoryError::UnknownRevision(revision))?;
        Ok(Application::new(
            revision.id,
            target.id,
            apply_transformation(&target.tree, target.id, &revision.transformation),
        ))
    }

    pub fn create_change(
        &mut self,
        dependencies: impl IntoIterator<Item = ChangeId>,
        transformation: Transformation,
    ) -> Result<(ChangeId, RevisionOutcome), RepositoryError> {
        let change = ChangeId::new();
        let dependencies = dependencies.into_iter().collect::<BTreeSet<_>>();
        for dependency in &dependencies {
            if !self.current_revisions.contains_key(dependency) {
                return Err(RepositoryError::UnknownDependency(*dependency));
            }
        }

        let before = self.clone();
        self.dependencies.add_change(change, dependencies);
        match self.add_revision(change, None, transformation) {
            Ok(outcome) => Ok((change, outcome)),
            Err(error) => {
                *self = before;
                Err(error)
            }
        }
    }

    pub fn revise_change(
        &mut self,
        change: ChangeId,
        predecessor: RevisionId,
        transformation: Transformation,
    ) -> Result<RevisionOutcome, RepositoryError> {
        if self.current_revision(change) != Some(predecessor) {
            return Err(if self.current_revisions.contains_key(&change) {
                RepositoryError::StaleRevision(change)
            } else {
                RepositoryError::UnknownChange(change)
            });
        }
        self.add_revision(change, Some(predecessor), transformation)
    }

    pub fn resolve_conflict(
        &mut self,
        resolution: ConflictResolution,
    ) -> Result<RevisionOutcome, RepositoryError> {
        let revision = self.revisions.get(&resolution.conflict_revision).ok_or(
            RepositoryError::UnknownRevision(resolution.conflict_revision),
        )?;
        let change = revision.change_id;
        if self.current_revision(change) != Some(resolution.conflict_revision) {
            return Err(RepositoryError::StaleRevision(change));
        }
        let application = self
            .conflicted_application(resolution.conflict_revision)
            .ok_or(RepositoryError::NotConflicted(resolution.conflict_revision))?;
        let ApplicationResult::Conflicted {
            partial_tree,
            conflict,
        } = &application.result
        else {
            unreachable!()
        };
        let conflict_paths = conflict
            .paths
            .iter()
            .map(|path| path.path.as_str())
            .collect::<BTreeSet<_>>();
        let changed_paths = changed_paths(partial_tree, &resolution.resolved_tree);
        if changed_paths
            .iter()
            .any(|path| !conflict_paths.contains(path.as_str()))
        {
            return Err(RepositoryError::InvalidConflictResolution);
        }

        let target = self.views[&application.target_view].clone();
        let transformation = diff_trees(&target.tree, &resolution.resolved_tree);
        self.add_revision(change, Some(resolution.conflict_revision), transformation)
    }

    pub fn publish(&mut self, expected: ViewId) -> Result<ViewId, RepositoryError> {
        if expected != self.published_view {
            return Err(RepositoryError::StalePublication {
                expected,
                actual: self.published_view,
            });
        }
        self.published_view = self.working_view;
        Ok(self.published_view)
    }

    fn add_revision(
        &mut self,
        change: ChangeId,
        predecessor: Option<RevisionId>,
        transformation: Transformation,
    ) -> Result<RevisionOutcome, RepositoryError> {
        let target = self.dependency_view(change)?;
        let result = apply_transformation(&target.tree, target.id, &transformation);
        let ApplicationResult::Clean {
            tree: authored_tree,
        } = &result
        else {
            return Err(RepositoryError::InvalidTransformation);
        };
        let revision_id = RevisionId::new();
        self.revisions.insert(
            revision_id,
            ChangeRevision {
                id: revision_id,
                change_id: change,
                predecessor,
                authored_against: target.id,
                transformation,
                resulting_tree: authored_tree.clone(),
            },
        );
        self.current_revisions.insert(change, revision_id);

        let reapplied = self
            .topological_order(&self.descendants(change))
            .into_iter()
            .filter(|descendant| *descendant != change)
            .collect::<Vec<_>>();
        self.rebuild_current_views()?;

        Ok(RevisionOutcome {
            revision_id,
            resulting_view: self.working_view,
            reapplied,
        })
    }

    fn rebuild_current_views(&mut self) -> Result<(), RepositoryError> {
        self.current_applications.clear();
        let all = self
            .current_revisions
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        for change in self.topological_order(&all) {
            let target = self.dependency_view(change)?;
            let revision_id = self.current_revisions[&change];
            let revision = &self.revisions[&revision_id];
            let application = Application::new(
                revision_id,
                target.id,
                apply_transformation(&target.tree, target.id, &revision.transformation),
            );
            self.applications
                .insert(application.id, application.clone());
            self.current_applications.insert(change, application);
        }
        let working = self.compose(&all)?;
        self.working_view = working.id;
        Ok(())
    }

    fn dependency_view(&mut self, change: ChangeId) -> Result<RepositoryView, RepositoryError> {
        let parents = self
            .dependencies
            .parents(change)
            .ok_or(RepositoryError::UnknownChange(change))?
            .clone();
        let mut closure = BTreeSet::new();
        for parent in parents {
            self.collect_ancestors(parent, &mut closure)?;
        }
        self.compose(&closure)
    }

    fn compose(
        &mut self,
        selected: &BTreeSet<ChangeId>,
    ) -> Result<RepositoryView, RepositoryError> {
        let mut tree = self.views[&self.base_view].tree.clone();
        let mut frontier = BTreeMap::new();
        let mut target_id = self.base_view;
        let mut applications = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        for change in self.topological_order(selected) {
            let revision_id = self
                .current_revision(change)
                .ok_or(RepositoryError::UnknownChange(change))?;
            let revision = &self.revisions[&revision_id];
            let result = apply_transformation(&tree, target_id, &revision.transformation);
            let application = Application::new(revision_id, target_id, result.clone());
            if let ApplicationResult::Conflicted { conflict, .. } = &result {
                conflicts.insert(conflict.id);
            }
            tree = result.materialized_tree().clone();
            frontier.insert(change, revision_id);
            applications.insert(change, application.id);
            self.applications.insert(application.id, application);
            let intermediate = RepositoryView::from_manifest(
                tree.clone(),
                frontier.clone(),
                applications.clone(),
                conflicts.clone(),
            );
            target_id = intermediate.id;
            self.views.insert(intermediate.id, intermediate);
        }
        let view = RepositoryView::from_manifest(tree, frontier, applications, conflicts);
        self.views.insert(view.id, view.clone());
        Ok(view)
    }

    fn collect_ancestors(
        &self,
        change: ChangeId,
        collected: &mut BTreeSet<ChangeId>,
    ) -> Result<(), RepositoryError> {
        if !collected.insert(change) {
            return Ok(());
        }
        let parents = self
            .dependencies
            .parents(change)
            .ok_or(RepositoryError::UnknownChange(change))?;
        for parent in parents {
            self.collect_ancestors(*parent, collected)?;
        }
        Ok(())
    }

    fn descendants(&self, root: ChangeId) -> BTreeSet<ChangeId> {
        let mut descendants = BTreeSet::from([root]);
        let mut pending = vec![root];
        while let Some(change) = pending.pop() {
            for child in self.dependencies.children(change) {
                if descendants.insert(child) {
                    pending.push(child);
                }
            }
        }
        descendants
    }

    fn topological_order(&self, selected: &BTreeSet<ChangeId>) -> Vec<ChangeId> {
        let mut remaining_parents = selected
            .iter()
            .map(|change| {
                let count = self
                    .dependencies
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
            for child in self.dependencies.children(change) {
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

fn apply_transformation(
    tree: &Tree,
    base_view: ViewId,
    transformation: &Transformation,
) -> ApplicationResult {
    let mut result = tree.clone();
    let mut conflicts = Vec::new();
    for edit in &transformation.edits {
        let actual = result.get(&edit.path).copied();
        if actual != edit.expected {
            conflicts.push(PathConflict {
                path: edit.path.clone(),
                expected: edit.expected,
                actual,
                proposed: edit.replacement,
            });
            continue;
        }
        match edit.replacement {
            Some(content) => {
                result.insert(edit.path.clone(), content);
            }
            None => {
                result.remove(&edit.path);
            }
        }
    }
    if conflicts.is_empty() {
        ApplicationResult::Clean { tree: result }
    } else {
        ApplicationResult::Conflicted {
            partial_tree: result,
            conflict: Conflict::new(base_view, conflicts),
        }
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

fn diff_trees(before: &Tree, after: &Tree) -> Transformation {
    Transformation {
        edits: changed_paths(before, after)
            .into_iter()
            .map(|path| crate::FileEdit {
                expected: before.get(&path).copied(),
                replacement: after.get(&path).copied(),
                path,
            })
            .collect(),
    }
}
