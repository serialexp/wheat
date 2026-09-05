use std::collections::BTreeSet;
use wheat_core::*;

#[test]
fn hydration_rejects_mismatched_tree_and_conflict_shape() {
    let mut repo = Repository::new(Tree::new());
    let (_, outcome) = repo
        .create_change(
            [],
            Transformation {
                edits: vec![FileEdit {
                    path: "file".into(),
                    expected: None,
                    replacement: Some(ContentId::of(b"value")),
                }],
            },
        )
        .unwrap();
    let revision = repo.revision(outcome.revision_id).unwrap();
    let record = decode_revision_record(&revision.canonical_bytes()).unwrap();
    assert!(hydrate_revision(record.clone(), Tree::new()).is_err());
    let hydrated = hydrate_revision(record, revision.resulting_tree().clone()).unwrap();
    assert_eq!(hydrated.canonical_bytes(), revision.canonical_bytes());
    let application = repo
        .applications_for_revision(outcome.revision_id)
        .next()
        .unwrap();
    let record = decode_application_record(&application.canonical_bytes()).unwrap();
    assert!(hydrate_application(record.clone(), Tree::new(), None).is_err());
    let hydrated =
        hydrate_application(record, application.result.materialized_tree().clone(), None).unwrap();
    assert_eq!(hydrated, *application);
}

#[test]
fn reconstruction_rejects_duplicate_and_missing_view_roots() {
    let repo = Repository::new(Tree::new());
    let state =
        decode_repository_state_record(&repo.canonical_closure().state.canonical_bytes()).unwrap();
    let parts = |views| RepositoryParts {
        state,
        changes: vec![],
        views,
        revisions: vec![],
        applications: vec![],
    };
    assert!(Repository::from_parts(parts(vec![])).is_err());
    assert!(
        Repository::from_parts(parts(vec![
            repo.working_view().clone(),
            repo.working_view().clone()
        ]))
        .is_err()
    );
    let restored = Repository::from_parts(parts(vec![repo.working_view().clone()])).unwrap();
    assert_eq!(restored.canonical_closure(), repo.canonical_closure());
    assert_eq!(
        restored.working_view().unresolved_conflicts,
        BTreeSet::new()
    );
}
