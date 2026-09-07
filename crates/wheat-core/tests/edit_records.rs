use wheat_core::{
    CanonicalObjectKind, ContentId, EditOperation, EditRecord, FileKind, FileValue,
    decode_edit_record, validate_canonical_object,
};

fn guarded() -> EditRecord {
    EditRecord::new(
        "src/main.rs",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(b"before\n"),
            expected: ContentId::of(b"old\n"),
            replacement: ContentId::of(b"new\n"),
            after: ContentId::of(b"after\n"),
            start_anchor: false,
            end_anchor: false,
        },
    )
    .unwrap()
}

#[test]
fn guarded_definition_roundtrips_and_every_precondition_affects_identity() {
    let original = guarded();
    let bytes = original.canonical_bytes();
    assert_eq!(decode_edit_record(&bytes).unwrap(), original);
    assert_eq!(
        validate_canonical_object(&bytes).unwrap(),
        CanonicalObjectKind::Edit
    );
    let mut changed = original.clone();
    changed.path = "src/other.rs".into();
    assert_ne!(changed.id(), original.id());
    for field in 0..7 {
        let mut changed = original.clone();
        let EditOperation::GuardedText {
            kind,
            before,
            expected,
            replacement,
            after,
            start_anchor,
            end_anchor,
        } = &mut changed.operation
        else {
            unreachable!()
        };
        match field {
            0 => *kind = FileKind::Executable,
            1 => *before = ContentId::of(b"different"),
            2 => *expected = ContentId::of(b"different"),
            3 => *replacement = ContentId::of(b"different"),
            4 => *after = ContentId::of(b"different"),
            5 => *start_anchor = true,
            _ => *end_anchor = true,
        }
        assert_ne!(changed.id(), original.id());
        assert_eq!(
            decode_edit_record(&changed.canonical_bytes()).unwrap(),
            changed
        );
    }
}

#[test]
fn exact_file_operations_preserve_kinds_and_reject_noops() {
    let regular = FileValue {
        content: ContentId::of(b"same"),
        kind: FileKind::Regular,
    };
    let executable = FileValue {
        kind: FileKind::Executable,
        ..regular
    };
    for operation in [
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(regular),
        },
        EditOperation::ExactFile {
            expected: Some(regular),
            replacement: None,
        },
        EditOperation::ExactFile {
            expected: Some(regular),
            replacement: Some(executable),
        },
    ] {
        let edit = EditRecord::new("file", operation).unwrap();
        assert_eq!(decode_edit_record(&edit.canonical_bytes()).unwrap(), edit);
    }
    assert!(
        EditRecord::new(
            "file",
            EditOperation::ExactFile {
                expected: None,
                replacement: None
            }
        )
        .is_err()
    );
    assert!(
        EditRecord::new(
            "file",
            EditOperation::ExactFile {
                expected: Some(regular),
                replacement: Some(regular)
            }
        )
        .is_err()
    );
}

#[test]
fn invalid_paths_symlink_text_and_damaged_encoding_are_rejected() {
    for path in ["", "/absolute", "../escape", "a/../b", "a//b", "a/./b"] {
        assert!(
            EditRecord::new(path, guarded().operation).is_err(),
            "accepted {path:?}"
        );
    }
    let mut edit = guarded();
    let EditOperation::GuardedText { kind, .. } = &mut edit.operation else {
        unreachable!()
    };
    *kind = FileKind::Symlink;
    assert!(EditRecord::new(edit.path, edit.operation).is_err());
    let bytes = guarded().canonical_bytes();
    for end in 0..bytes.len() {
        assert!(decode_edit_record(&bytes[..end]).is_err());
    }
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_edit_record(&extra).is_err());
}
