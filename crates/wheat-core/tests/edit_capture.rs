use wheat_core::{
    ContentId, EditOperation, FileKind,
    edit_capture::{
        CaptureLimits, EditCaptureError, FileSnapshot, SnapshotSide, capture_edit,
        capture_edit_with_limits,
    },
    guarded::{Limits, authoring::AuthoringError},
};

fn file<'a>(kind: FileKind, bytes: &'a [u8]) -> FileSnapshot<'a> {
    FileSnapshot { kind, bytes }
}

#[test]
fn normalized_inputs_have_identical_identity() {
    let captured = capture_edit(
        "src/main.rs",
        Some(file(FileKind::Regular, b"one\r\ntwo\rthree\n")),
        Some(file(FileKind::Regular, b"one\ntwo\nthree\n")),
        None,
        Limits::default(),
    )
    .unwrap();

    assert_eq!(captured.before, captured.after);
    assert!(captured.edits.is_empty());
    let value = captured.before.unwrap();
    assert_eq!(value.content, ContentId::of(b"one\ntwo\nthree\n"));
    assert_eq!(captured.content[&value.content], b"one\ntwo\nthree\n");
}

#[test]
fn binary_and_symlink_snapshots_are_byte_exact() {
    for (kind, before, after) in [
        (FileKind::Regular, &b"a\0\r\nb"[..], &b"a\0\rb"[..]),
        (
            FileKind::Symlink,
            &b"dir\r\ntarget"[..],
            &b"dir\rtarget"[..],
        ),
    ] {
        let captured = capture_edit(
            "link-or-data",
            Some(file(kind, before)),
            Some(file(kind, after)),
            None,
            Limits::default(),
        )
        .unwrap();
        assert_eq!(captured.content[&ContentId::of(before)], before);
        assert_eq!(captured.content[&ContentId::of(after)], after);
        assert!(matches!(
            captured.edits.values().next().unwrap().operation,
            EditOperation::ExactFile { .. }
        ));
    }
}

#[test]
fn text_above_guarded_bound_is_an_exact_replacement_with_a_separate_file_bound() {
    let before = b"alpha\nbeta\ngamma\n";
    let after = b"alpha\nchanged\ngamma\n";
    let captured = capture_edit_with_limits(
        "large.txt",
        Some(file(FileKind::Regular, before)),
        Some(file(FileKind::Regular, after)),
        None,
        CaptureLimits {
            max_file_bytes: after.len(),
            max_guarded_lines: wheat_core::guarded::line_diff::DEFAULT_MAX_LINES,
            guarded: Limits {
                max_input_bytes: before.len() - 1,
                ..Limits::default()
            },
        },
    )
    .unwrap();

    let EditOperation::ExactFile {
        expected,
        replacement,
    } = captured.edits.values().next().unwrap().operation
    else {
        panic!("oversized text transition must be exact")
    };
    assert_eq!(expected.unwrap().content, ContentId::of(before));
    assert_eq!(replacement.unwrap().content, ContentId::of(after));
}

#[test]
fn text_above_line_diff_bound_is_an_exact_replacement() {
    let before = "a\n".repeat(wheat_core::guarded::line_diff::DEFAULT_MAX_LINES + 1);
    let mut after = before.clone();
    after.replace_range(..1, "b");
    let captured = capture_edit_with_limits(
        "many-lines.txt",
        Some(file(FileKind::Regular, before.as_bytes())),
        Some(file(FileKind::Regular, after.as_bytes())),
        None,
        CaptureLimits {
            max_file_bytes: after.len(),
            max_guarded_lines: wheat_core::guarded::line_diff::DEFAULT_MAX_LINES,
            guarded: Limits {
                max_input_bytes: after.len(),
                max_work: usize::MAX,
                ..Limits::default()
            },
        },
    )
    .unwrap();

    assert!(matches!(
        captured.edits.values().next().unwrap().operation,
        EditOperation::ExactFile { .. }
    ));
}

#[test]
fn separate_file_bound_still_rejects_oversized_exact_content() {
    let error = capture_edit_with_limits(
        "large.bin",
        None,
        Some(file(FileKind::Regular, b"large\0binary")),
        None,
        CaptureLimits {
            max_file_bytes: 4,
            max_guarded_lines: wheat_core::guarded::line_diff::DEFAULT_MAX_LINES,
            guarded: Limits::default(),
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        EditCaptureError::InputTooLarge {
            side: SnapshotSide::After,
            actual: 12,
            limit: 4
        }
    ));
}

#[test]
fn mode_only_change_is_an_explicit_exact_edit() {
    let captured = capture_edit(
        "script",
        Some(file(FileKind::Regular, b"echo yes\r\n")),
        Some(file(FileKind::Executable, b"echo yes\n")),
        None,
        Limits::default(),
    )
    .unwrap();

    assert_eq!(captured.edits.len(), 1);
    let EditOperation::ExactFile {
        expected,
        replacement,
    } = captured.edits.values().next().unwrap().operation
    else {
        panic!("mode transition must be exact")
    };
    assert_eq!(expected.unwrap().kind, FileKind::Regular);
    assert_eq!(replacement.unwrap().kind, FileKind::Executable);
    assert_eq!(expected.unwrap().content, replacement.unwrap().content);
}

#[test]
fn create_delete_and_binary_use_exact_edits() {
    let created = capture_edit(
        "new",
        None,
        Some(file(FileKind::Regular, b"new\r\n")),
        None,
        Limits::default(),
    )
    .unwrap();
    assert!(matches!(
        created.edits.values().next().unwrap().operation,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(_)
        }
    ));

    let deleted = capture_edit(
        "old",
        Some(file(FileKind::Regular, b"old\n")),
        None,
        None,
        Limits::default(),
    )
    .unwrap();
    assert!(matches!(
        deleted.edits.values().next().unwrap().operation,
        EditOperation::ExactFile {
            expected: Some(_),
            replacement: None
        }
    ));
}

#[test]
fn derives_many_independent_guarded_text_edits() {
    let mut before = String::new();
    let mut after = String::new();
    for line in 0..500 {
        before.push_str(&format!("stable line {line:03}\n"));
        if line % 31 == 7 && line < 31 * 16 {
            after.push_str(&format!("changed line {line:03}\n"));
        } else {
            after.push_str(&format!("stable line {line:03}\n"));
        }
    }
    let captured = capture_edit(
        "many.txt",
        Some(file(FileKind::Regular, before.as_bytes())),
        Some(file(FileKind::Regular, after.as_bytes())),
        None,
        Limits::default(),
    )
    .unwrap();

    assert_eq!(captured.edits.len(), 16);
    assert!(
        captured
            .edits
            .values()
            .all(|record| matches!(record.operation, EditOperation::GuardedText { .. }))
    );
}

#[test]
fn authoring_budget_failure_never_falls_back_to_exact() {
    let before = b"alpha\nbeta\ngamma\n";
    let after = b"alpha\nchanged\ngamma\n";
    let error = capture_edit(
        "budget.txt",
        Some(file(FileKind::Regular, before)),
        Some(file(FileKind::Regular, after)),
        None,
        Limits {
            max_input_bytes: 1024,
            max_edits: 10,
            max_work: 0,
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        EditCaptureError::Authoring(AuthoringError::Diff(_))
    ));
}

#[test]
fn final_newline_change_is_guarded_and_preserved() {
    let captured = capture_edit(
        "newline.txt",
        Some(file(FileKind::Regular, b"last line")),
        Some(file(FileKind::Regular, b"last line\r\n")),
        None,
        Limits::default(),
    )
    .unwrap();

    assert!(
        captured
            .edits
            .values()
            .all(|record| matches!(record.operation, EditOperation::GuardedText { .. }))
    );
    assert_eq!(
        captured.content[&captured.after.unwrap().content],
        b"last line\n"
    );
}

#[test]
fn zero_edit_budget_rejects_changes_but_allows_noop() {
    let limits = Limits {
        max_edits: 0,
        ..Limits::default()
    };
    let exact = capture_edit(
        "mode",
        Some(file(FileKind::Regular, b"same\n")),
        Some(file(FileKind::Executable, b"same\n")),
        None,
        limits,
    )
    .unwrap_err();
    assert!(matches!(
        exact,
        EditCaptureError::TooManyEdits {
            actual: 1,
            limit: 0
        }
    ));

    let text = capture_edit(
        "text",
        Some(file(FileKind::Regular, b"before\n")),
        Some(file(FileKind::Regular, b"after\n")),
        None,
        limits,
    )
    .unwrap_err();
    assert!(matches!(
        text,
        EditCaptureError::Authoring(AuthoringError::Guarded(
            wheat_core::guarded::Error::TooManyEdits { limit: 0, .. }
        ))
    ));

    let noop = capture_edit(
        "noop",
        Some(file(FileKind::Regular, b"same\r\n")),
        Some(file(FileKind::Regular, b"same\n")),
        None,
        limits,
    )
    .unwrap();
    assert!(noop.edits.is_empty());
}

#[test]
fn rejects_nonempty_explicit_guards_for_unsupported_transitions() {
    let range = std::iter::once(0..0).collect::<Vec<_>>();
    let cases = [
        (
            Some(file(FileKind::Regular, &b"same\n"[..])),
            Some(file(FileKind::Regular, &b"same\n"[..])),
        ),
        (
            Some(file(FileKind::Regular, &b"a\0"[..])),
            Some(file(FileKind::Regular, &b"b\0"[..])),
        ),
        (
            Some(file(FileKind::Symlink, &b"old"[..])),
            Some(file(FileKind::Symlink, &b"new"[..])),
        ),
        (None, Some(file(FileKind::Regular, &b"new\n"[..]))),
        (Some(file(FileKind::Regular, &b"old\n"[..])), None),
        (
            Some(file(FileKind::Regular, &b"same\n"[..])),
            Some(file(FileKind::Executable, &b"same\n"[..])),
        ),
    ];

    for (before, after) in cases {
        let error = capture_edit(
            "unsupported",
            before,
            after,
            Some(&range),
            Limits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, EditCaptureError::UnsupportedExplicitGuards));
    }
}

#[test]
fn rejects_path_bounds_and_guard_ranges_with_typed_errors() {
    let error = capture_edit(
        "../escape",
        Some(file(FileKind::Regular, &[0; 32])),
        None,
        None,
        Limits {
            max_input_bytes: 1,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, EditCaptureError::InvalidPath(_)));

    let error = capture_edit(
        "large",
        Some(file(FileKind::Regular, &[0; 2])),
        None,
        None,
        Limits {
            max_input_bytes: 1,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        EditCaptureError::InputTooLarge {
            side: SnapshotSide::Before,
            actual: 2,
            limit: 1
        }
    ));

    let invalid_ranges = [0..3, 0..0];
    let error = capture_edit(
        "guard",
        Some(file(FileKind::Regular, b"a\n")),
        Some(file(FileKind::Regular, b"b\n")),
        Some(&invalid_ranges),
        Limits::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        EditCaptureError::InvalidGuardRange { index: 0 }
    ));
}
