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
    fn store_generated(&self, id: ContentId, b: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().insert(id, b.to_vec());
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
    for (id, b) in &captured.content {
        io.store_generated(*id, b).unwrap();
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
    repo.create_change([], edits, ApplicationContext::new(&io))
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
fn empty_repository_closure_contains_root_state_view_tree_and_manifest() {
    let repository = Repository::new(Tree::new());
    let closure = repository.canonical_closure();
    let kinds = closure
        .objects
        .iter()
        .map(|object| validate_canonical_object(&object.bytes).unwrap())
        .collect::<Vec<_>>();

    // Four semantic objects, four empty view collections, the empty change
    // lookup, an empty retained root, and one compressed retained leaf naming
    // the base view.
    assert_eq!(closure.objects.len(), 11);
    assert!(kinds.contains(&CanonicalObjectKind::RepositoryState));
    assert!(kinds.contains(&CanonicalObjectKind::View));
    assert!(kinds.contains(&CanonicalObjectKind::Tree));
    assert!(kinds.contains(&CanonicalObjectKind::ContentManifest));
    assert!(closure.required_content.is_empty());
}

#[test]
fn closure_is_deterministic_sorted_and_deduplicated() {
    let io = Memory::default();
    let ctx = ApplicationContext::new(&io);
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change([], exact(&io, "a", None, Some("a1")), ctx)
        .unwrap();
    repository
        .create_change([], exact(&io, "b", None, Some("b1")), ctx)
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
    let (a, a1) = repository
        .create_change([], exact(&io, "shared", Some("base"), Some("a1")), ctx)
        .unwrap();
    let (_, b1) = repository
        .create_change([a], exact(&io, "shared", Some("a1"), Some("b1")), ctx)
        .unwrap();
    let old = repository
        .revision(a1.revision_id)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect();
    repository
        .revise_change(
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
    let (change, first) = repository
        .create_change([], exact(&io, "file", None, Some("one")), ctx)
        .unwrap();
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
            change,
            first.revision_id,
            replacement(&io, old, "file", None, Some("two")),
            ctx,
        )
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
