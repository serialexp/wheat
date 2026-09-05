use std::collections::BTreeSet;

use wheat_core::{
    CanonicalObjectKind, ContentId, FileEdit, Repository, Transformation, Tree,
    validate_canonical_object,
};

fn content(value: &str) -> ContentId {
    ContentId::of(value.as_bytes())
}

fn edit(path: &str, expected: Option<&str>, replacement: Option<&str>) -> Transformation {
    Transformation {
        edits: vec![FileEdit {
            path: path.into(),
            expected: expected.map(content),
            replacement: replacement.map(content),
        }],
    }
}

#[test]
fn empty_repository_closure_contains_root_state_view_tree_and_manifest() {
    let repository = Repository::new(Tree::new());
    let closure = repository.canonical_closure();
    let kinds = closure
        .objects
        .iter()
        .map(|object| validate_canonical_object(&object.bytes).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(closure.objects.len(), 4);
    assert!(kinds.contains(&CanonicalObjectKind::RepositoryState));
    assert!(kinds.contains(&CanonicalObjectKind::View));
    assert!(kinds.contains(&CanonicalObjectKind::Tree));
    assert!(kinds.contains(&CanonicalObjectKind::ContentManifest));
    assert!(closure.required_content.is_empty());
}

#[test]
fn closure_is_deterministic_sorted_and_deduplicated() {
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], edit("a", None, Some("a1")))
        .unwrap();
    repository
        .create_change([], edit("b", None, Some("b1")))
        .unwrap();

    let first = repository.canonical_closure();
    let second = repository.canonical_closure();
    assert_eq!(first, second);
    assert!(
        first
            .objects
            .windows(2)
            .all(|objects| objects[0].id < objects[1].id)
    );
    assert!(first.objects.iter().all(|object| {
        wheat_core::ObjectId::of_canonical_bytes(&object.bytes) == object.id
            && validate_canonical_object(&object.bytes).is_ok()
    }));
}

#[test]
fn aggregate_manifest_retains_revision_and_conflict_terms() {
    let mut base = Tree::new();
    base.insert("shared".into(), content("base"));
    let mut repository = Repository::new(base);
    let (a, a1) = repository
        .create_change([], edit("shared", Some("base"), Some("a1")))
        .unwrap();
    repository
        .create_change([a], edit("shared", Some("a1"), Some("b1")))
        .unwrap();
    repository
        .revise_change(a, a1.revision_id, edit("shared", Some("base"), Some("a2")))
        .unwrap();

    let closure = repository.canonical_closure();
    assert_eq!(
        closure.required_content,
        BTreeSet::from([content("base"), content("a1"), content("a2"), content("b1"),])
    );
    assert!(closure.objects.iter().any(|object| {
        validate_canonical_object(&object.bytes) == Ok(CanonicalObjectKind::Conflict)
    }));
}

#[test]
fn closure_retains_superseded_revisions_and_historical_applications() {
    let mut repository = Repository::new(Tree::new());
    let (change, first) = repository
        .create_change([], edit("file", None, Some("one")))
        .unwrap();
    let applications_before = repository
        .applications_for_revision(first.revision_id)
        .count();
    let second = repository
        .revise_change(change, first.revision_id, edit("file", None, Some("two")))
        .unwrap();
    let closure = repository.canonical_closure();

    assert!(
        closure
            .state
            .retained_revisions
            .contains(&first.revision_id)
    );
    assert!(
        closure
            .state
            .retained_revisions
            .contains(&second.revision_id)
    );
    assert!(closure.state.retained_applications.len() > applications_before);
    assert_eq!(
        closure
            .objects
            .iter()
            .filter(|object| {
                validate_canonical_object(&object.bytes) == Ok(CanonicalObjectKind::Revision)
            })
            .count(),
        2
    );
}
