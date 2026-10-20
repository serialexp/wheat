use std::collections::BTreeMap;

use wheat_core::edit_application::{ApplicationLimits, ApplicationOutcome, apply_edits};
use wheat_core::{
    ContentId, EditOperation, EditRecord, FileKind, FileValue, Tree, VerifiedContent,
};

fn blob(bytes: &[u8]) -> (ContentId, VerifiedContent) {
    let content = VerifiedContent::new(bytes.to_vec());
    (content.id(), content)
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
    let output = result.generated_content[&result.tree["f"].content].bytes();
    assert_eq!(output, b"header\nONE\nmiddle\nTWO\nfooter\n");
}

/// Resolved content arrives verified, so applying edits hashes only what it generates.
#[test]
fn application_hashes_only_generated_content() {
    let (source_id, source) = blob(&b"line\n".repeat(256));
    let blobs: BTreeMap<_, _> = [
        (source_id, source),
        blob(b""),
        blob(b"line\n"),
        blob(b"LINE\n"),
    ]
    .into();
    let edit = EditRecord::new(
        "f",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(b""),
            expected: ContentId::of(b"line\n"),
            replacement: ContentId::of(b"LINE\n"),
            after: ContentId::of(b""),
            start_anchor: true,
            end_anchor: false,
        },
    )
    .unwrap();
    let tree: Tree = [(
        "f".into(),
        FileValue {
            content: source_id,
            kind: FileKind::Regular,
        },
    )]
    .into();

    let before = wheat_core::content_bytes_hashed();
    let result = apply_edits(
        &tree,
        [(edit.id(), edit)],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();
    let hashed = wheat_core::content_bytes_hashed() - before;

    let generated = result
        .generated_content
        .values()
        .map(|content| content.len() as u64)
        .sum::<u64>();
    assert_eq!(generated, 256 * 5);
    assert_eq!(hashed, generated);
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
        |_| Ok::<_, &str>(VerifiedContent::new(b"wrong".to_vec())),
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
fn exact_and_guarded_reapplication_recognize_already_satisfied_edits() {
    let final_bytes = b"top\nfinal\nbottom\n";
    let final_id = ContentId::of(final_bytes);
    let exact = EditRecord::new(
        "exact",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: final_id,
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let guarded = EditRecord::new(
        "guarded",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(b"top\n"),
            expected: ContentId::of(b"middle\n"),
            replacement: ContentId::of(b"final\n"),
            after: ContentId::of(b"bottom\n"),
            start_anchor: true,
            end_anchor: true,
        },
    )
    .unwrap();
    let tree = BTreeMap::from([
        (
            "exact".into(),
            FileValue {
                content: final_id,
                kind: FileKind::Regular,
            },
        ),
        (
            "guarded".into(),
            FileValue {
                content: final_id,
                kind: FileKind::Regular,
            },
        ),
    ]);
    let blobs = BTreeMap::from([
        blob(final_bytes),
        blob(b"top\n"),
        blob(b"middle\n"),
        blob(b"final\n"),
        blob(b"bottom\n"),
    ]);
    let limits = ApplicationLimits {
        allow_already_satisfied: true,
        ..ApplicationLimits::default()
    };
    let result = apply_edits(
        &tree,
        [(exact.id(), exact.clone()), (guarded.id(), guarded.clone())],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        limits,
    )
    .unwrap();
    assert_eq!(result.tree, tree);
    assert_eq!(
        result.outcomes[&exact.id()],
        ApplicationOutcome::AlreadySatisfied { read: None }
    );
    assert!(matches!(
        result.outcomes[&guarded.id()],
        ApplicationOutcome::AlreadySatisfied { read: Some(_) }
    ));
    assert!(result.generated_content.is_empty());

    let strict = apply_edits(
        &tree,
        [(exact.id(), exact)],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        strict.outcomes.values().next().unwrap(),
        ApplicationOutcome::ExpectedFileMismatch { .. }
    ));
}

#[test]
fn aggregate_work_budget_is_shared_across_paths() {
    let source = b"abc";
    let source_id = ContentId::of(source);
    let replacement = b"x";
    let blobs: BTreeMap<_, _> = [blob(source), blob(b"a"), blob(replacement), blob(b"")].into();
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
    assert_eq!(
        result.generated_content[&result.tree["a"].content].bytes(),
        b"xbc"
    );
    assert_eq!(result.tree["a"], result.tree["b"]);
}

#[test]
fn interference_ids_and_outcome_order_are_canonical() {
    let source = b"abc";
    let source_id = ContentId::of(source);
    let blobs: BTreeMap<_, _> =
        [blob(source), blob(b"a"), blob(b"ab"), blob(b"x"), blob(b"")].into();
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

#[test]
fn one_revision_applies_more_edits_than_one_file_may_hold() {
    // The per-file guarded hunk bound must not cap the revision: a large initial import
    // creates thousands of files, and a sweeping change edits thousands of existing ones.
    let files = wheat_core::guarded::DEFAULT_MAX_EDITS + 904;
    let (old_id, old) = blob(b"old\n");
    let (new_id, new) = blob(b"new\n");
    let (empty_id, empty) = blob(b"");
    let blobs: BTreeMap<_, _> = [(old_id, old), (new_id, new), (empty_id, empty)].into();
    let regular = |content| FileValue {
        content,
        kind: FileKind::Regular,
    };
    let mut target = Tree::new();
    let mut edits = Vec::with_capacity(files * 2);
    for index in 0..files {
        let existing = format!("existing/{index:05}");
        target.insert(existing.clone(), regular(old_id));
        let guarded = EditRecord::new(
            existing,
            EditOperation::GuardedText {
                kind: FileKind::Regular,
                before: empty_id,
                expected: old_id,
                replacement: new_id,
                after: empty_id,
                start_anchor: true,
                end_anchor: true,
            },
        )
        .unwrap();
        let created = EditRecord::new(
            format!("created/{index:05}"),
            EditOperation::ExactFile {
                expected: None,
                replacement: Some(regular(new_id)),
            },
        )
        .unwrap();
        edits.push((guarded.id(), guarded));
        edits.push((created.id(), created));
    }

    let result = apply_edits(
        &target,
        edits,
        |id| blobs.get(&id).cloned().ok_or("missing"),
        ApplicationLimits::default(),
    )
    .unwrap();

    assert_eq!(result.outcomes.len(), files * 2);
    assert!(
        result
            .outcomes
            .values()
            .all(|outcome| matches!(outcome, ApplicationOutcome::Applied { .. }))
    );
    assert_eq!(result.tree.len(), files * 2);
    assert!(result.tree.values().all(|value| value.content == new_id));
}
