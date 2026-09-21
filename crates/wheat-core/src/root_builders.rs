use crate::{
    CanonicalObject, ContentId, ContentManifestRecord, ObjectId, RepositoryState,
    RepositoryStateRecord, RepositoryView, ViewRecord,
};
use std::collections::BTreeSet;

pub fn build_content_manifest_record(
    content: &BTreeSet<ContentId>,
    sink: &mut impl FnMut(CanonicalObject),
) -> ContentManifestRecord {
    // Build a flat sorted snapshot blob instead of a radix trie.
    let members: Vec<ContentId> = content.iter().copied().collect();
    let bytes = crate::content_set_snapshot_bytes(&members);
    let id = crate::ContentSetId::of_canonical_bytes(&bytes);
    sink(CanonicalObject {
        id: id.into(),
        bytes,
    });
    ContentManifestRecord { content: id }
}

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
    let manifest = build_content_manifest_record(&view.required_content, sink);
    let required_content = manifest.id();
    sink(CanonicalObject {
        id: required_content.into(),
        bytes: manifest.canonical_bytes(),
    });
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
