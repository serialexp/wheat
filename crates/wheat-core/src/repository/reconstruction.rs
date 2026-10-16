use super::*;

impl Repository {
    /// Reconstruct an executable graph from persisted records and all referenced objects.
    pub fn from_parts(parts: RepositoryParts) -> Result<Self, RepositoryError> {
        let mut changes = BTreeMap::new();
        for change in parts.changes {
            let id = change.id;
            if changes.insert(id, change).is_some() {
                return Err(RepositoryError::InvalidRepositoryState(
                    "duplicate change identity",
                ));
            }
        }

        let mut views = BTreeMap::new();
        for view in parts.views {
            let id = view.id;
            if id != ViewId::of_canonical_bytes(&view.canonical_bytes()) {
                return Err(RepositoryError::InvalidRepositoryState("view identity"));
            }
            if views.insert(id, view).is_some() {
                return Err(RepositoryError::InvalidRepositoryState(
                    "duplicate view identity",
                ));
            }
        }

        let mut revisions = BTreeMap::new();
        for (record, tree, edits) in parts.revisions {
            let revision = crate::hydrate_revision(record, tree, edits)?;
            let id = revision.id();
            if revisions.insert(id, revision).is_some() {
                return Err(RepositoryError::InvalidRepositoryState(
                    "duplicate revision identity",
                ));
            }
        }

        let mut applications = BTreeMap::new();
        for (record, tree, conflict) in parts.applications {
            let application = crate::hydrate_application(record, tree, conflict)?;
            let id = application.id;
            if applications.insert(id, application).is_some() {
                return Err(RepositoryError::InvalidRepositoryState(
                    "duplicate application identity",
                ));
            }
        }

        let repository = Self {
            changes,
            views,
            revisions,
            applications,
            base_view: parts.state.base_view,
            view_lineage: parts.view_lineage,
        };
        repository.validate()?;
        repository.validate_references()?;
        Ok(repository)
    }

    fn validate_references(&self) -> Result<(), RepositoryError> {
        for revision in self.revisions.values() {
            if !self.changes.contains_key(&revision.change_id) {
                return Err(RepositoryError::InvalidRepositoryState(
                    "revision change reference",
                ));
            }
            if revision
                .predecessor
                .iter()
                .chain(&revision.combines)
                .any(|id| !self.revisions.contains_key(id))
                || !self.views.contains_key(&revision.authored_against)
            {
                return Err(RepositoryError::InvalidRepositoryState(
                    "revision object reference",
                ));
            }
            if revision
                .dependencies
                .iter()
                .any(|id| !self.changes.contains_key(id))
            {
                return Err(RepositoryError::InvalidRepositoryState(
                    "revision dependency reference",
                ));
            }
        }
        for application in self.applications.values() {
            if !self.revisions.contains_key(&application.revision_id)
                || !self.views.contains_key(&application.target_view)
            {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application object reference",
                ));
            }
            let has_resulting_edge = self.view_lineage.iter().any(|(parent, child)| {
                *parent == application.target_view
                    && self.views.get(child).is_some_and(|view| {
                        view.applications.values().any(|id| *id == application.id)
                    })
            });
            if !has_resulting_edge {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application view lineage reference",
                ));
            }
        }
        for view in self.views.values() {
            if view.applications.keys().ne(view.frontier.keys()) {
                return Err(RepositoryError::InvalidRepositoryState(
                    "view application selection",
                ));
            }
            for (change, revision) in &view.frontier {
                if self.revisions.get(revision).map(ChangeRevision::change_id) != Some(*change) {
                    return Err(RepositoryError::InvalidRepositoryState(
                        "view revision reference",
                    ));
                }
            }
            for (change, application) in &view.applications {
                let Some(application) = self.applications.get(application) else {
                    return Err(RepositoryError::InvalidRepositoryState(
                        "view application reference",
                    ));
                };
                if self
                    .revisions
                    .get(&application.revision_id)
                    .map(ChangeRevision::change_id)
                    != Some(*change)
                {
                    return Err(RepositoryError::InvalidRepositoryState(
                        "view application change reference",
                    ));
                }
            }
            for conflict_id in &view.unresolved_conflicts {
                if !self.applications.values().any(|application| {
                    matches!(&application.result, ApplicationResult::Conflicted { conflict, .. } if conflict.id == *conflict_id)
                }) {
                    return Err(RepositoryError::InvalidRepositoryState("view conflict reference"));
                }
            }
        }
        Ok(())
    }
}
