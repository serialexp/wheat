use std::collections::BTreeMap;

use wheat_core::{
    ApplicationResult, ConflictResolution, ContentId, FileEdit, Repository, RepositoryError,
    Transformation, Tree,
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
fn revising_a_reapplies_b_and_c_without_replacing_change_identity() {
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], edit("a", None, Some("a1")))
        .unwrap();
    let (b, b1) = repository
        .create_change([a], edit("b", None, Some("b1")))
        .unwrap();
    let (c, c1) = repository
        .create_change([b], edit("c", None, Some("c1")))
        .unwrap();

    let outcome = repository
        .revise_change(a, a1.revision_id, edit("a", None, Some("a2")))
        .unwrap();

    assert_eq!(outcome.reapplied, vec![b, c]);
    assert_eq!(repository.current_revision(b), Some(b1.revision_id));
    assert_eq!(repository.current_revision(c), Some(c1.revision_id));
    assert_eq!(
        repository.working_view().tree.get("a"),
        Some(&content("a2"))
    );
    assert_eq!(
        repository.working_view().tree.get("b"),
        Some(&content("b1"))
    );
    assert_eq!(
        repository.working_view().tree.get("c"),
        Some(&content("c1"))
    );
}

#[test]
fn conflict_is_durable_and_resolution_continues_descendant_application() {
    let mut base = Tree::new();
    base.insert("shared".into(), content("base"));
    let mut repository = Repository::new(base);
    let (a, a1) = repository
        .create_change([], edit("shared", Some("base"), Some("a1")))
        .unwrap();
    let (b, b1) = repository
        .create_change([a], edit("shared", Some("a1"), Some("b1")))
        .unwrap();
    let (c, _) = repository
        .create_change([b], edit("c", None, Some("c1")))
        .unwrap();

    let revised = repository
        .revise_change(a, a1.revision_id, edit("shared", Some("base"), Some("a2")))
        .unwrap();
    assert_eq!(revised.reapplied, vec![b, c]);

    let application = repository
        .applications_for_revision(b1.revision_id)
        .find(|application| matches!(application.result, ApplicationResult::Conflicted { .. }))
        .expect("reapplication records a durable conflict");
    let ApplicationResult::Conflicted { conflict, .. } = &application.result else {
        unreachable!()
    };
    assert_eq!(conflict.paths.len(), 1);
    assert_eq!(conflict.paths[0].actual, Some(content("a2")));
    assert_eq!(conflict.paths[0].proposed, Some(content("b1")));

    let mut resolved = repository
        .view(application.target_view)
        .unwrap()
        .tree
        .clone();
    resolved.insert("shared".into(), content("resolved"));
    let outcome = repository
        .resolve_conflict(ConflictResolution {
            conflict_revision: b1.revision_id,
            resolved_tree: resolved,
        })
        .unwrap();
    assert_eq!(outcome.reapplied, vec![c]);
    assert_eq!(
        repository.working_view().tree.get("shared"),
        Some(&content("resolved"))
    );
    assert_eq!(
        repository.working_view().tree.get("c"),
        Some(&content("c1"))
    );
    assert_eq!(repository.conflicts().count(), 0);
}

#[test]
fn one_revision_can_be_applied_to_unrelated_target_views() {
    let mut repository = Repository::new(Tree::new());
    let (_, created) = repository
        .create_change([], edit("new", None, Some("value")))
        .unwrap();
    let first_target = repository.published_view().clone();
    let mut unrelated_tree = Tree::new();
    unrelated_tree.insert("unrelated".into(), content("other"));
    let unrelated = wheat_core::RepositoryView::new(unrelated_tree, BTreeMap::new());

    let first = repository
        .apply_revision(created.revision_id, &first_target)
        .unwrap();
    let second = repository
        .apply_revision(created.revision_id, &unrelated)
        .unwrap();

    assert!(matches!(first.result, ApplicationResult::Clean { .. }));
    assert!(matches!(second.result, ApplicationResult::Clean { .. }));
    assert_ne!(first.target_view, second.target_view);
}

#[test]
fn independent_roots_remain_composed_in_the_working_view() {
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], edit("a", None, Some("a")))
        .unwrap();
    repository
        .create_change([], edit("b", None, Some("b")))
        .unwrap();

    assert_eq!(repository.working_view().tree.get("a"), Some(&content("a")));
    assert_eq!(repository.working_view().tree.get("b"), Some(&content("b")));
}

#[test]
fn sibling_branches_are_applied_to_their_declared_parent_not_each_other() {
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], edit("base", None, Some("a1")))
        .unwrap();
    let (b, _) = repository
        .create_change([a], edit("shared", None, Some("b")))
        .unwrap();
    let (c, _) = repository
        .create_change([a], edit("shared", None, Some("c")))
        .unwrap();

    repository
        .revise_change(a, a1.revision_id, edit("base", None, Some("a2")))
        .unwrap();

    assert!(matches!(
        repository.current_application(b).unwrap().result,
        ApplicationResult::Clean { .. }
    ));
    assert!(matches!(
        repository.current_application(c).unwrap().result,
        ApplicationResult::Clean { .. }
    ));
}

#[test]
fn join_change_composes_both_dependency_closures() {
    let mut repository = Repository::new(Tree::new());
    let (a, _) = repository
        .create_change([], edit("a", None, Some("a")))
        .unwrap();
    let (b, _) = repository
        .create_change([], edit("b", None, Some("b")))
        .unwrap();
    repository
        .create_change([a, b], edit("join", None, Some("joined")))
        .unwrap();

    assert_eq!(repository.working_view().tree.get("a"), Some(&content("a")));
    assert_eq!(repository.working_view().tree.get("b"), Some(&content("b")));
    assert_eq!(
        repository.working_view().tree.get("join"),
        Some(&content("joined"))
    );
}

#[test]
fn descendant_reapplication_order_is_topological_not_breadth_first() {
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], edit("a", None, Some("a1")))
        .unwrap();
    let (b, _) = repository
        .create_change([a], edit("b", None, Some("b")))
        .unwrap();
    let (c, _) = repository
        .create_change([b], edit("c", None, Some("c")))
        .unwrap();
    let (d, _) = repository
        .create_change([a, c], edit("d", None, Some("d")))
        .unwrap();

    let outcome = repository
        .revise_change(a, a1.revision_id, edit("a", None, Some("a2")))
        .unwrap();

    assert_eq!(outcome.reapplied, vec![b, c, d]);
}

#[test]
fn failed_creation_does_not_leave_a_descendant_that_panics_on_reapplication() {
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], edit("a", None, Some("a1")))
        .unwrap();
    assert_eq!(
        repository.create_change([a], edit("missing", Some("nope"), Some("value"))),
        Err(RepositoryError::InvalidTransformation)
    );

    repository
        .revise_change(a, a1.revision_id, edit("a", None, Some("a2")))
        .unwrap();
    assert_eq!(repository.current_revisions().count(), 1);
}

#[test]
fn resolving_a_stale_conflicted_revision_is_rejected() {
    let mut base = Tree::new();
    base.insert("shared".into(), content("base"));
    let mut repository = Repository::new(base);
    let (a, a1) = repository
        .create_change([], edit("shared", Some("base"), Some("a1")))
        .unwrap();
    let (b, b1) = repository
        .create_change([a], edit("shared", Some("a1"), Some("b1")))
        .unwrap();
    repository
        .revise_change(a, a1.revision_id, edit("shared", Some("base"), Some("a2")))
        .unwrap();
    let target = repository
        .conflicted_application(b1.revision_id)
        .unwrap()
        .target_view;
    let mut resolved = repository.view(target).unwrap().tree.clone();
    resolved.insert("shared".into(), content("resolved"));
    let b2 = repository
        .resolve_conflict(ConflictResolution {
            conflict_revision: b1.revision_id,
            resolved_tree: resolved.clone(),
        })
        .unwrap();

    assert_eq!(
        repository.resolve_conflict(ConflictResolution {
            conflict_revision: b1.revision_id,
            resolved_tree: resolved,
        }),
        Err(RepositoryError::StaleRevision(b))
    );
    assert_eq!(repository.current_revision(b), Some(b2.revision_id));
}

#[test]
fn publication_is_compare_and_swap() {
    let mut repository = Repository::new(BTreeMap::new());
    let original = repository.published_view().id;
    repository
        .create_change([], edit("a", None, Some("a")))
        .unwrap();
    let published = repository.publish(original).unwrap();
    repository
        .create_change([], edit("b", None, Some("b")))
        .unwrap();

    assert_eq!(
        repository.publish(original),
        Err(RepositoryError::StalePublication {
            expected: original,
            actual: published,
        })
    );
}
