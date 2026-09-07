use std::collections::BTreeMap;

use wheat_core::edit_application::{ApplicationLimits, ApplicationOutcome, apply_edits};
use wheat_core::{ContentId, EditOperation, EditRecord, FileKind, FileValue, Tree};

fn blob(bytes: &[u8]) -> (ContentId, Vec<u8>) {
    (ContentId::of(bytes), bytes.to_vec())
}

#[test]
fn independent_moved_guards_apply_against_original() {
    let (source_id, source) = blob(b"header\none\nmiddle\ntwo\nfooter\n");
    let parts = [
        b"header\n".as_slice(),
        b"one\n",
        b"ONE\n",
        b"middle\n",
        b"two\n",
        b"TWO\n",
        b"footer\n",
    ];
    let mut blobs: BTreeMap<_, _> = parts.into_iter().map(blob).collect();
    blobs.insert(source_id, source);
    let mk = |before: &[u8], expected: &[u8], replacement: &[u8], after: &[u8]| {
        EditRecord::new(
            "f",
            EditOperation::GuardedText {
                kind: FileKind::Regular,
                before: ContentId::of(before),
                expected: ContentId::of(expected),
                replacement: ContentId::of(replacement),
                after: ContentId::of(after),
                start_anchor: false,
                end_anchor: false,
            },
        )
        .unwrap()
    };
    let a = mk(b"header\n", b"one\n", b"ONE\n", b"middle\n");
    let b = mk(b"middle\n", b"two\n", b"TWO\n", b"footer\n");
    let tree: Tree = [(
        "f".into(),
        FileValue {
            content: source_id,
            kind: FileKind::Regular,
        },
    )]
    .into();
    let result = apply_edits(
        &tree,
        [(b.id(), b.clone()), (a.id(), a.clone())],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        result.outcomes[&a.id()],
        ApplicationOutcome::Applied { .. }
    ));
    assert!(matches!(
        result.outcomes[&b.id()],
        ApplicationOutcome::Applied { .. }
    ));
    let output = result.generated_content[&result.tree["f"].content].as_slice();
    assert_eq!(output, b"header\nONE\nmiddle\nTWO\nfooter\n");
}

#[test]
fn exact_mode_only_change_and_corrupt_content() {
    let (id, bytes) = blob(b"same\n");
    let old = FileValue {
        content: id,
        kind: FileKind::Regular,
    };
    let new = FileValue {
        content: id,
        kind: FileKind::Executable,
    };
    let edit = EditRecord::new(
        "f",
        EditOperation::ExactFile {
            expected: Some(old),
            replacement: Some(new),
        },
    )
    .unwrap();
    let tree: Tree = [("f".into(), old)].into();
    let result = apply_edits(
        &tree,
        [(edit.id(), edit.clone())],
        |_| Ok::<_, &str>(bytes.clone()),
        ApplicationLimits::default(),
    )
    .unwrap();
    assert_eq!(result.tree["f"], new);

    let error = apply_edits(
        &tree,
        [(edit.id(), edit)],
        |_| Ok::<_, &str>(b"wrong".to_vec()),
        ApplicationLimits::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        wheat_core::edit_application::ApplicationError::CorruptContent { .. }
    ));
}

#[test]
fn exact_symlink_replacement_uses_full_file_value() {
    let old_bytes = b"destination-a";
    let new_bytes = b"destination-b";
    let old = FileValue {
        content: ContentId::of(old_bytes),
        kind: FileKind::Symlink,
    };
    let new = FileValue {
        content: ContentId::of(new_bytes),
        kind: FileKind::Symlink,
    };
    let edit = EditRecord::new(
        "link",
        EditOperation::ExactFile {
            expected: Some(old),
            replacement: Some(new),
        },
    )
    .unwrap();
    let tree: Tree = [("link".into(), old)].into();
    let blobs: BTreeMap<_, _> = [blob(old_bytes), blob(new_bytes)].into();
    let result = apply_edits(
        &tree,
        [(edit.id(), edit.clone())],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();
    assert_eq!(result.tree["link"], new);
    assert!(matches!(
        result.outcomes[&edit.id()],
        ApplicationOutcome::Applied {
            read: None,
            write: None
        }
    ));
    assert!(result.generated_content.is_empty());
}

#[test]
fn aggregate_work_budget_is_shared_across_paths() {
    let source = b"abc";
    let source_id = ContentId::of(source);
    let replacement = b"x";
    let mut blobs: BTreeMap<_, _> = [blob(source), blob(b"a"), blob(replacement)].into();
    blobs.insert(ContentId::of(b""), Vec::new());
    let make = |path: &str, anchored| {
        EditRecord::new(
            path,
            EditOperation::GuardedText {
                kind: FileKind::Regular,
                before: ContentId::of(b""),
                expected: ContentId::of(b"a"),
                replacement: ContentId::of(replacement),
                after: ContentId::of(b""),
                start_anchor: anchored,
                end_anchor: false,
            },
        )
        .unwrap()
    };
    let cheap = make("a", true); // one byte comparison
    let costly = make("b", false); // match, then first-byte rejection: two comparisons
    let value = FileValue {
        content: source_id,
        kind: FileKind::Regular,
    };
    let tree: Tree = [("a".into(), value), ("b".into(), value)].into();
    let limits = ApplicationLimits {
        guarded: wheat_core::guarded::Limits {
            max_work: 4,
            ..wheat_core::guarded::Limits::default()
        },
        ..ApplicationLimits::default()
    };
    let result = apply_edits(
        &tree,
        [(costly.id(), costly.clone()), (cheap.id(), cheap.clone())],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        limits,
    )
    .unwrap();
    assert!(matches!(
        result.outcomes[&cheap.id()],
        ApplicationOutcome::Applied { .. }
    ));
    assert!(matches!(
        result.outcomes[&costly.id()],
        ApplicationOutcome::Applied { .. }
    ));
    assert_eq!(result.generated_content.len(), 1);
    assert_eq!(result.generated_content[&result.tree["a"].content], b"xbc");
    assert_eq!(result.tree["a"], result.tree["b"]);
}

#[test]
fn interference_ids_and_outcome_order_are_canonical() {
    let source = b"abc";
    let source_id = ContentId::of(source);
    let mut blobs: BTreeMap<_, _> = [blob(source), blob(b"a"), blob(b"ab"), blob(b"x")].into();
    blobs.insert(ContentId::of(b""), Vec::new());
    let make = |expected: &[u8]| {
        EditRecord::new(
            "f",
            EditOperation::GuardedText {
                kind: FileKind::Regular,
                before: ContentId::of(b""),
                expected: ContentId::of(expected),
                replacement: ContentId::of(b"x"),
                after: ContentId::of(b""),
                start_anchor: true,
                end_anchor: false,
            },
        )
        .unwrap()
    };
    let short = make(b"a");
    let long = make(b"ab");
    let tree: Tree = [(
        "f".into(),
        FileValue {
            content: source_id,
            kind: FileKind::Regular,
        },
    )]
    .into();
    let result = apply_edits(
        &tree,
        [(long.id(), long.clone()), (short.id(), short.clone())],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();
    assert_eq!(result.tree, tree);
    assert_eq!(
        result.outcomes[&short.id()],
        ApplicationOutcome::Interference {
            with: vec![long.id()]
        }
    );
    assert_eq!(
        result.outcomes[&long.id()],
        ApplicationOutcome::Interference {
            with: vec![short.id()]
        }
    );
    assert!(result.outcomes.keys().copied().is_sorted());
}
