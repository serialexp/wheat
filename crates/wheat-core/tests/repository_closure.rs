use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};
use wheat_core::*;
#[derive(Default)]
struct Memory(RefCell<BTreeMap<ContentId, Vec<u8>>>);
impl ContentIo for Memory {
    type Error = &'static str;
    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        let bytes = self.0.borrow().get(&id).cloned().ok_or("missing")?;
        VerifiedContent::check(id, bytes).map_err(|_| "corrupt")
    }
    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error> {
        self.0
            .borrow_mut()
            .insert(content.id(), content.bytes().to_vec());
        Ok(())
    }
}
fn value(io: &Memory, bytes: &str) -> FileValue {
    let content = ContentId::of(bytes.as_bytes());
    io.0.borrow_mut().insert(content, bytes.as_bytes().to_vec());
    FileValue {
        content,
        kind: FileKind::Regular,
    }
}
fn exact(
    io: &Memory,
    path: &str,
    expected: Option<&str>,
    replacement: Option<&str>,
) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(|bytes| value(io, bytes)),
            replacement: replacement.map(|bytes| value(io, bytes)),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: BTreeMap::from([(id, edit)]),
        transition: EditTransition {
            added: BTreeSet::from([id]),
            ..Default::default()
        },
    }
}
fn replacement(
    io: &Memory,
    old: BTreeSet<EditId>,
    path: &str,
    expected: Option<&str>,
    replacement: Option<&str>,
) -> RevisionEdits {
    let mut edits = exact(io, path, expected, replacement);
    edits.transition.removed = old;
    edits
}
#[test]
fn closure_emits_standalone_definitions_and_retains_fragments() {
    let io = Memory::default();
    for b in [
        b"before\nold\nafter\n".as_slice(),
        b"before\nnew\nafter\n".as_slice(),
    ] {
        io.0.borrow_mut().insert(ContentId::of(b), b.to_vec());
    }
    let captured = edit_capture::capture_edit(
        "f",
        Some(edit_capture::FileSnapshot {
            kind: FileKind::Regular,
            bytes: b"before\nold\nafter\n",
        }),
        Some(edit_capture::FileSnapshot {
            kind: FileKind::Regular,
            bytes: b"before\nnew\nafter\n",
        }),
        None,
        guarded::Limits::default(),
    )
    .unwrap();
    for content in captured.content.values() {
        io.store_generated(content.borrowed()).unwrap();
    }
    let ids = captured.edits.keys().copied().collect::<BTreeSet<_>>();
    let edits = RevisionEdits {
        edits: captured.edits,
        transition: EditTransition {
            added: ids.clone(),
            ..Default::default()
        },
    };
    let mut repo = Repository::new(Tree::from_iter([("f".into(), captured.before.unwrap())]));
    let effective = repo.base_view().id;
    repo.create_change(
        repo.base_view().id,
        effective,
        [],
        edits,
        ApplicationContext::new(&io),
    )
    .unwrap();
    let closure = repo.canonical_closure();
    for id in ids {
        assert!(closure.objects.iter().any(|o| o.id == id.into()));
    }
    assert!(
        captured
            .content
            .keys()
            .all(|id| closure.required_content.contains(id))
    );
    assert!(closure.objects.windows(2).all(|w| w[0].id < w[1].id));
}

#[test]
fn empty_repository_closure_contains_root_state_view_tree_and_content_roots() {
    let repository = Repository::new(Tree::new());
    let closure = repository.canonical_closure();
    let kinds = closure
        .objects
        .iter()
        .map(|object| validate_canonical_object(&object.bytes).unwrap())
        .collect::<Vec<_>>();

    // Three semantic objects, five empty view collections (the content map and the
    // tips among them), the empty aggregate content set, the empty change lookup, an empty
    // view-lineage map, an empty retained root, one compressed retained leaf
    // naming the base view, and the empty projected-commit set.
    assert_eq!(closure.objects.len(), 14);
    assert!(kinds.contains(&CanonicalObjectKind::ChangeSetNode));
    assert!(kinds.contains(&CanonicalObjectKind::ProjectedCommitSetNode));
    assert!(kinds.contains(&CanonicalObjectKind::RepositoryState));
    assert!(kinds.contains(&CanonicalObjectKind::View));
    assert!(kinds.contains(&CanonicalObjectKind::Tree));
    assert!(kinds.contains(&CanonicalObjectKind::ContentMapNode));
    assert!(kinds.contains(&CanonicalObjectKind::ContentSetNode));
    assert!(closure.required_content.is_empty());
}

#[test]
fn closure_is_deterministic_sorted_and_deduplicated() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let effective = repository.base_view().id;
    repository
        .create_change(
            repository.base_view().id,
            effective,
            [],
            exact(&io, "a", None, Some("a1")),
            ctx,
        )
        .unwrap();
    repository
        .create_change(
            repository.base_view().id,
            effective,
            [],
            exact(&io, "b", None, Some("b1")),
            ctx,
        )
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
        ObjectId::of_canonical_bytes(&object.bytes) == object.id
            && validate_canonical_object(&object.bytes).is_ok()
    }));
}

#[test]
fn aggregate_manifest_retains_revision_and_conflict_terms() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut base = Tree::new();
    base.insert("shared".into(), value(&io, "base"));
    let mut repository = Repository::new(base);
    let mut effective = repository.base_view().id;
    let (a, a1) = repository
        .create_change(
            repository.base_view().id,
            effective,
            [],
            exact(&io, "shared", Some("base"), Some("a1")),
            ctx,
        )
        .unwrap();
    effective = a1.resulting_view;
    let (_, b1) = repository
        .create_change(
            repository.base_view().id,
            effective,
            [a],
            exact(&io, "shared", Some("a1"), Some("b1")),
            ctx,
        )
        .unwrap();
    effective = b1.resulting_view;
    let old = repository
        .revision(a1.revision_id)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect();
    repository
        .revise_change(
            repository.base_view().id,
            effective,
            a,
            a1.revision_id,
            replacement(&io, old, "shared", Some("base"), Some("a2")),
            ctx,
        )
        .unwrap();

    let closure = repository.canonical_closure();
    assert_eq!(
        closure.required_content,
        BTreeSet::from([
            ContentId::of(b"base"),
            ContentId::of(b"a1"),
            ContentId::of(b"a2"),
            ContentId::of(b"b1"),
        ])
    );
    assert!(closure.objects.iter().any(|object| {
        validate_canonical_object(&object.bytes) == Ok(CanonicalObjectKind::Conflict)
    }));
    let application = repository
        .applications_for_revision(b1.revision_id)
        .find(|application| matches!(application.result, ApplicationResult::Conflicted { .. }))
        .expect("durable conflict");
    let ApplicationResult::Conflicted { conflict, .. } = &application.result else {
        unreachable!()
    };
    assert_eq!(conflict.edits.len(), 1);
    let evidence = conflict.edits.values().next().unwrap();
    assert_eq!(evidence.target, Some(value(&io, "a2")));
    assert_eq!(evidence.reason, EditFailure::ExpectedFileMismatch);
}

#[test]
fn closure_retains_superseded_revisions_and_historical_applications() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    let mut effective = repository.base_view().id;
    let (change, first) = repository
        .create_change(
            repository.base_view().id,
            effective,
            [],
            exact(&io, "file", None, Some("one")),
            ctx,
        )
        .unwrap();
    effective = first.resulting_view;
    let applications_before = repository
        .applications_for_revision(first.revision_id)
        .count();
    let old = repository
        .revision(first.revision_id)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect();
    let second = repository
        .revise_change(
            repository.base_view().id,
            effective,
            change,
            first.revision_id,
            replacement(&io, old, "file", None, Some("two")),
            ctx,
        )
        .unwrap();
    let closure = repository.canonical_closure();
    let empty_lineage = build_view_lineage_map([], |_| {});

    assert_ne!(closure.state.view_lineage, empty_lineage);
    assert!(closure.state.retained_views.contains(&first.resulting_view));
    assert!(
        closure
            .state
            .retained_views
            .contains(&second.resulting_view)
    );
    assert!(closure.objects.iter().any(|object| {
        validate_canonical_object(&object.bytes) == Ok(CanonicalObjectKind::ViewLineageMapNode)
    }));
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
