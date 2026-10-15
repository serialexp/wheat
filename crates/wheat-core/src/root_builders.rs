use crate::{
    CanonicalObject, ObjectId, RepositoryState, RepositoryStateRecord, RepositoryView, ViewRecord,
};

pub fn build_view_record(
    view: &RepositoryView,
    sink: &mut impl FnMut(CanonicalObject),
) -> ViewRecord {
    let revisions =
        crate::build_revision_map(view.frontier.iter().map(|(k, v)| (*k, *v)), &mut *sink);
    let applications =
        crate::build_application_map(view.applications.iter().map(|(k, v)| (*k, *v)), &mut *sink);
    let unresolved_conflicts =
        crate::build_conflict_set(view.unresolved_conflicts.iter().copied(), &mut *sink);
    let required_content = crate::build_content_map(
        view.required_content
            .iter()
            .map(|(content, count)| (*content, *count)),
        sink,
    );
    ViewRecord {
        tree: crate::wire::tree_id(&view.tree),
        revisions,
        applications,
        unresolved_conflicts,
        required_content,
    }
}

pub fn build_repository_state_record(
    state: &RepositoryState,
    sink: &mut impl FnMut(CanonicalObject),
) -> RepositoryStateRecord {
    RepositoryStateRecord {
        base_view: state.base_view,
        changes: crate::build_change_map(
            state
                .change_objects
                .iter()
                .map(|(change, object)| (*change, *object)),
            &mut *sink,
        ),
        view_lineage: state.view_lineage,
        required_content: state.required_content,
        retained_changes: crate::build_retained_set(
            state.retained_changes.iter().copied().map(ObjectId::from),
            &mut *sink,
        ),
        retained_views: crate::build_retained_set(
            state.retained_views.iter().copied().map(ObjectId::from),
            &mut *sink,
        ),
        retained_revisions: crate::build_retained_set(
            state.retained_revisions.iter().copied().map(ObjectId::from),
            &mut *sink,
        ),
        retained_applications: crate::build_retained_set(
            state
                .retained_applications
                .iter()
                .copied()
                .map(ObjectId::from),
            sink,
        ),
    }
}
