use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};
use wheat_core::*;

#[derive(Default)]
struct Memory(RefCell<BTreeMap<ContentId, Vec<u8>>>);
impl ContentIo for Memory {
    type Error = &'static str;
    fn resolve(&self, id: ContentId) -> Result<Vec<u8>, Self::Error> {
        self.0.borrow().get(&id).cloned().ok_or("missing")
    }
    fn store_generated(&self, id: ContentId, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().insert(id, bytes.to_vec());
        Ok(())
    }
}
fn value(io: &Memory, bytes: &str) -> FileValue {
    let id = ContentId::of(bytes.as_bytes());
    io.0.borrow_mut().insert(id, bytes.as_bytes().to_vec());
    FileValue {
        content: id,
        kind: FileKind::Regular,
    }
}
fn exact(io: &Memory, path: &str, before: Option<&str>, after: Option<&str>) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: before.map(|v| value(io, v)),
            replacement: after.map(|v| value(io, v)),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: BTreeMap::from([(id, record)]),
        transition: EditTransition {
            added: BTreeSet::from([id]),
            ..Default::default()
        },
    }
}
fn replacement(
    io: &Memory,
    old: &BTreeSet<EditId>,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> RevisionEdits {
    let mut next = exact(io, path, before, after);
    next.transition.removed = old.clone();
    next
}
fn proposal(
    io: &Memory,
    id: ChangeId,
    dependencies: impl IntoIterator<Item = ChangeId>,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> ChangeProposal {
    ChangeProposal {
        id,
        metadata: ChangeMetadata::default(),
        dependencies: dependencies.into_iter().collect(),
        revision_metadata: RevisionMetadata::default(),
        edits: exact(io, path, before, after),
    }
}
fn current_edits(repository: &Repository, change: ChangeId) -> BTreeSet<EditId> {
    repository
        .revision(repository.current_revision(change).unwrap())
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect()
}

#[test]
fn bulk_import_materializes_each_commit_view_and_the_combined_working_view() {
    let io = Memory::default();
    let (root, left, right) = (ChangeId::new(), ChangeId::new(), ChangeId::new());
    let mut repository = Repository::new(Tree::new());
    let outcomes = repository
        .import_changes(
            [
                proposal(&io, root, [], "root", None, Some("root")),
                proposal(&io, left, [root], "left", None, Some("left")),
                proposal(&io, right, [root], "right", None, Some("right")),
            ],
            ApplicationContext::new(&io),
        )
        .unwrap();
    assert_eq!(
        repository
            .view(outcomes[&root].resulting_view)
            .unwrap()
            .tree
            .len(),
        1
    );
    assert!(
        repository
            .view(outcomes[&left].resulting_view)
            .unwrap()
            .tree
            .contains_key("left")
    );
    assert!(
        !repository
            .view(outcomes[&left].resulting_view)
            .unwrap()
            .tree
            .contains_key("right")
    );
    assert_eq!(repository.working_view().tree.len(), 3);
    repository.validate().unwrap();
}

#[test]
fn failed_bulk_import_rolls_back_the_whole_batch() {
    let io = Memory::default();
    let mut repository = Repository::new(Tree::new());
    let before = repository.canonical_closure();
    let (valid, missing) = (ChangeId::new(), ChangeId::new());
    let error = repository
        .import_changes(
            [
                proposal(&io, valid, [], "valid", None, Some("valid")),
                proposal(
                    &io,
                    ChangeId::new(),
                    [missing],
                    "invalid",
                    None,
                    Some("invalid"),
                ),
            ],
            ApplicationContext::new(&io),
        )
        .unwrap_err();
    assert_eq!(error, RepositoryError::UnknownDependency(missing));
    assert_eq!(repository.canonical_closure(), before);
    assert_eq!(repository.current_revision(valid), None);
}

#[test]
fn revising_a_reapplies_b_and_c_without_replacing_change_identity() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], exact(&io, "a", None, Some("a1")), ctx)
        .unwrap();
    let (b, b1) = repository
        .create_change([a], exact(&io, "b", None, Some("b1")), ctx)
        .unwrap();
    let (c, c1) = repository
        .create_change([b], exact(&io, "c", None, Some("c1")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    let outcome = repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "a", None, Some("a2")),
            ctx,
        )
        .unwrap();
    assert_eq!(outcome.reapplied, vec![b, c]);
    assert_eq!(repository.current_revision(b), Some(b1.revision_id));
    assert_eq!(repository.current_revision(c), Some(c1.revision_id));
    assert_eq!(
        repository.working_view().tree.get("a"),
        Some(&value(&io, "a2"))
    );
    assert_eq!(
        repository.working_view().tree.get("b"),
        Some(&value(&io, "b1"))
    );
    assert_eq!(
        repository.working_view().tree.get("c"),
        Some(&value(&io, "c1"))
    );
}

#[test]
fn conflict_is_durable_and_resolution_continues_descendant_application() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::from_iter([("shared".into(), value(&io, "base"))]));
    let (a, a1) = repository
        .create_change([], exact(&io, "shared", Some("base"), Some("a1")), ctx)
        .unwrap();
    let (b, b1) = repository
        .create_change([a], exact(&io, "shared", Some("a1"), Some("b1")), ctx)
        .unwrap();
    let (c, _) = repository
        .create_change([b], exact(&io, "c", None, Some("c1")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    let revised = repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "shared", Some("base"), Some("a2")),
            ctx,
        )
        .unwrap();
    assert_eq!(revised.reapplied, vec![b, c]);
    let application = repository
        .applications_for_revision(b1.revision_id)
        .find(|a| matches!(a.result, ApplicationResult::Conflicted { .. }))
        .expect("durable conflict");
    let ApplicationResult::Conflicted { conflict, .. } = &application.result else {
        unreachable!()
    };
    assert_eq!(conflict.edits.len(), 1);
    let evidence = conflict.edits.values().next().unwrap();
    assert_eq!(evidence.target, Some(value(&io, "a2")));
    assert_eq!(evidence.reason, EditFailure::ExpectedFileMismatch);
    let mut resolved = repository
        .view(application.target_view)
        .unwrap()
        .tree
        .clone();
    resolved.insert("shared".into(), value(&io, "resolved"));
    let outcome = repository
        .resolve_conflict(
            ConflictResolution {
                conflict_revision: b1.revision_id,
                resolved_tree: resolved,
            },
            ctx,
        )
        .unwrap();
    assert_eq!(outcome.reapplied, vec![c]);
    assert_eq!(
        repository.working_view().tree.get("shared"),
        Some(&value(&io, "resolved"))
    );
    assert_eq!(
        repository.working_view().tree.get("c"),
        Some(&value(&io, "c1"))
    );
    assert_eq!(repository.conflicts().count(), 0);
    assert!(repository.working_view().unresolved_conflicts.is_empty());
}

#[test]
fn conflicted_view_is_self_describing_and_queryable() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::from_iter([("shared".into(), value(&io, "base"))]));
    let (a, a1) = repository
        .create_change([], exact(&io, "shared", Some("base"), Some("a1")), ctx)
        .unwrap();
    let (b, b1) = repository
        .create_change([a], exact(&io, "shared", Some("a1"), Some("b1")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "shared", Some("base"), Some("a2")),
            ctx,
        )
        .unwrap();
    let view = repository.working_view();
    assert_eq!(view.unresolved_conflicts.len(), 1);
    assert_eq!(view.applications.len(), 2);
    assert!(view.required_content.contains(&value(&io, "a2").content));
    assert_eq!(repository.conflicts_in_view(view.id).unwrap().count(), 1);
    assert_eq!(
        repository.conflicts_for_change(view.id, b).unwrap().count(),
        1
    );
    assert_eq!(
        repository
            .conflicts_for_path(view.id, "shared")
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        repository.evaluate_no_unresolved_conflicts(view.id),
        TargetPolicyResult::Rejected {
            unresolved_conflicts: 1
        }
    );
    assert_eq!(repository.current_revision(b), Some(b1.revision_id));
}

#[test]
fn view_identity_covers_the_application_and_conflict_manifest() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let tree = BTreeMap::from([("file".into(), value(&io, "visible"))]);
    let mut source = Repository::new(Tree::new());
    let (change, outcome) = source
        .create_change([], exact(&io, "file", None, Some("visible")), ctx)
        .unwrap();
    let revision = outcome.revision_id;
    let frontier = BTreeMap::from([(change, revision)]);
    let plain = RepositoryView::new(tree.clone(), frontier.clone());
    let edit = source
        .revision(revision)
        .unwrap()
        .edits()
        .keys()
        .next()
        .copied()
        .unwrap();
    let conflict = Conflict::new(
        plain.id,
        BTreeMap::from([(
            edit,
            FailedEditEvidence {
                target: Some(value(&io, "visible")),
                reason: EditFailure::ExpectedFileMismatch,
            },
        )]),
    );
    let application = Application::new(
        revision,
        plain.id,
        ApplicationResult::Conflicted {
            partial_tree: tree.clone(),
            conflict: conflict.clone(),
        },
        BTreeMap::new(),
    );
    let manifested = RepositoryView::from_manifest(
        tree,
        frontier,
        BTreeMap::from([(change, application.id)]),
        BTreeSet::from([conflict.id]),
    );
    assert_ne!(plain.id, manifested.id);
    assert_eq!(plain.tree, manifested.tree);
}

#[test]
fn one_revision_can_be_applied_to_unrelated_target_views() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (_, created) = repository
        .create_change([], exact(&io, "new", None, Some("value")), ctx)
        .unwrap();
    let first_target = repository.published_view().clone();
    let unrelated = RepositoryView::new(
        Tree::from_iter([("unrelated".into(), value(&io, "other"))]),
        BTreeMap::new(),
    );
    let first = repository
        .apply_revision(created.revision_id, &first_target, ctx)
        .unwrap();
    let second = repository
        .apply_revision(created.revision_id, &unrelated, ctx)
        .unwrap();
    assert!(matches!(first.result, ApplicationResult::Clean { .. }));
    assert!(matches!(second.result, ApplicationResult::Clean { .. }));
    assert_ne!(first.target_view, second.target_view);
}

#[test]
fn independent_roots_remain_composed_in_the_working_view() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], exact(&io, "a", None, Some("a")), ctx)
        .unwrap();
    repository
        .create_change([], exact(&io, "b", None, Some("b")), ctx)
        .unwrap();
    assert_eq!(
        repository.working_view().tree.get("a"),
        Some(&value(&io, "a"))
    );
    assert_eq!(
        repository.working_view().tree.get("b"),
        Some(&value(&io, "b"))
    );
}

#[test]
fn sibling_branches_are_applied_to_their_declared_parent_not_each_other() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], exact(&io, "base", None, Some("a1")), ctx)
        .unwrap();
    let (b, _) = repository
        .create_change([a], exact(&io, "shared", None, Some("b")), ctx)
        .unwrap();
    let (c, _) = repository
        .create_change([a], exact(&io, "shared", None, Some("c")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "base", None, Some("a2")),
            ctx,
        )
        .unwrap();
    assert!(matches!(
        repository.current_application(b).unwrap().result,
        ApplicationResult::Clean { .. }
    ));
    assert!(matches!(
        repository.current_application(c).unwrap().result,
        ApplicationResult::Clean { .. }
    ));
    let working = repository.working_view().id;
    assert_eq!(
        repository.view(working).unwrap().unresolved_conflicts.len(),
        1
    );
    assert_eq!(repository.conflicts_in_view(working).unwrap().count(), 1);
    assert_eq!(
        repository
            .conflicts_for_path(working, "shared")
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        [b, c]
            .into_iter()
            .map(|change| repository
                .conflicts_for_change(working, change)
                .unwrap()
                .count())
            .sum::<usize>(),
        1
    );
}

#[test]
fn join_change_composes_both_dependency_closures() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (a, _) = repository
        .create_change([], exact(&io, "a", None, Some("a")), ctx)
        .unwrap();
    let (b, _) = repository
        .create_change([], exact(&io, "b", None, Some("b")), ctx)
        .unwrap();
    repository
        .create_change([a, b], exact(&io, "join", None, Some("joined")), ctx)
        .unwrap();
    assert_eq!(
        repository.working_view().tree.get("a"),
        Some(&value(&io, "a"))
    );
    assert_eq!(
        repository.working_view().tree.get("b"),
        Some(&value(&io, "b"))
    );
    assert_eq!(
        repository.working_view().tree.get("join"),
        Some(&value(&io, "joined"))
    );
}

#[test]
fn descendant_reapplication_order_is_topological_not_breadth_first() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], exact(&io, "a", None, Some("a1")), ctx)
        .unwrap();
    let (b, _) = repository
        .create_change([a], exact(&io, "b", None, Some("b")), ctx)
        .unwrap();
    let (c, _) = repository
        .create_change([b], exact(&io, "c", None, Some("c")), ctx)
        .unwrap();
    let (d, _) = repository
        .create_change([a, c], exact(&io, "d", None, Some("d")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    let outcome = repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "a", None, Some("a2")),
            ctx,
        )
        .unwrap();
    assert_eq!(outcome.reapplied, vec![b, c, d]);
}

#[test]
fn failed_creation_does_not_leave_a_descendant_that_panics_on_reapplication() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let (a, a1) = repository
        .create_change([], exact(&io, "a", None, Some("a1")), ctx)
        .unwrap();
    assert_eq!(
        repository.create_change([a], exact(&io, "missing", Some("nope"), Some("value")), ctx),
        Err(RepositoryError::InvalidEdits)
    );
    let old = current_edits(&repository, a);
    repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "a", None, Some("a2")),
            ctx,
        )
        .unwrap();
    assert_eq!(repository.current_revisions().count(), 1);
}

#[test]
fn resolving_a_stale_conflicted_revision_is_rejected() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::from_iter([("shared".into(), value(&io, "base"))]));
    let (a, a1) = repository
        .create_change([], exact(&io, "shared", Some("base"), Some("a1")), ctx)
        .unwrap();
    let (b, b1) = repository
        .create_change([a], exact(&io, "shared", Some("a1"), Some("b1")), ctx)
        .unwrap();
    let old = current_edits(&repository, a);
    repository
        .revise_change(
            a,
            a1.revision_id,
            replacement(&io, &old, "shared", Some("base"), Some("a2")),
            ctx,
        )
        .unwrap();
    let target = repository
        .conflicted_application(b1.revision_id)
        .unwrap()
        .target_view;
    let mut resolved = repository.view(target).unwrap().tree.clone();
    resolved.insert("shared".into(), value(&io, "resolved"));
    let b2 = repository
        .resolve_conflict(
            ConflictResolution {
                conflict_revision: b1.revision_id,
                resolved_tree: resolved.clone(),
            },
            ctx,
        )
        .unwrap();
    assert_eq!(
        repository.resolve_conflict(
            ConflictResolution {
                conflict_revision: b1.revision_id,
                resolved_tree: resolved
            },
            ctx
        ),
        Err(RepositoryError::StaleRevision(b))
    );
    assert_eq!(repository.current_revision(b), Some(b2.revision_id));
}

#[test]
fn publication_is_compare_and_swap() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(BTreeMap::new());
    let original = repository.published_view().id;
    repository
        .create_change([], exact(&io, "a", None, Some("a")), ctx)
        .unwrap();
    let published = repository.publish(original).unwrap();
    repository
        .create_change([], exact(&io, "b", None, Some("b")), ctx)
        .unwrap();
    assert_eq!(
        repository.publish(original),
        Err(RepositoryError::StalePublication {
            expected: original,
            actual: published
        })
    );
}
