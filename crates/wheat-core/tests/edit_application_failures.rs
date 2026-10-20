use std::collections::BTreeMap;
use wheat_core::edit_application::{
    ApplicationError, ApplicationLimits, ApplicationOutcome, apply_edits,
};
use wheat_core::{
    ContentId, EditOperation, EditRecord, FileKind, FileValue, Tree, VerifiedContent,
};

fn blob(bytes: &[u8]) -> (ContentId, VerifiedContent) {
    let content = VerifiedContent::new(bytes.to_vec());
    (content.id(), content)
}

fn fixture(
    source: &[u8],
    kind: FileKind,
) -> (Tree, EditRecord, BTreeMap<ContentId, VerifiedContent>) {
    let parts = [source, b"", b"old", b"new"];
    let blobs = parts.into_iter().map(blob).collect();
    let tree = [(
        "f".into(),
        FileValue {
            content: ContentId::of(source),
            kind,
        },
    )]
    .into();
    let edit = EditRecord::new(
        "f",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(b""),
            expected: ContentId::of(b"old"),
            replacement: ContentId::of(b"new"),
            after: ContentId::of(b""),
            start_anchor: false,
            end_anchor: false,
        },
    )
    .unwrap();
    (tree, edit, blobs)
}

#[test]
fn missing_ambiguous_and_kind_mismatch_preserve_target() {
    for (source, kind, expected) in [
        (
            b"absent".as_slice(),
            FileKind::Regular,
            ApplicationOutcome::NoMatch,
        ),
        (
            b"old old".as_slice(),
            FileKind::Regular,
            ApplicationOutcome::Ambiguous,
        ),
        (
            b"old".as_slice(),
            FileKind::Symlink,
            ApplicationOutcome::KindMismatch {
                expected: FileKind::Regular,
                actual: Some(FileKind::Symlink),
            },
        ),
    ] {
        let (tree, edit, blobs) = fixture(source, kind);
        let result = apply_edits(
            &tree,
            [(edit.id(), edit.clone())],
            |id| blobs.get(&id).cloned().ok_or("missing"),
            ApplicationLimits::default(),
        )
        .unwrap();
        assert_eq!(result.tree, tree);
        assert_eq!(result.outcomes[&edit.id()], expected);
        assert!(result.generated_content.is_empty());
    }
}

#[test]
fn missing_definition_is_fatal_even_on_kind_mismatch() {
    let (tree, edit, mut blobs) = fixture(b"old", FileKind::Symlink);
    blobs.remove(&ContentId::of(b"new"));
    assert!(matches!(
        apply_edits(
            &tree,
            [(edit.id(), edit)],
            |id| blobs.get(&id).cloned().ok_or("missing"),
            ApplicationLimits::default()
        ),
        Err(ApplicationError::Resolve { .. })
    ));
}

#[test]
fn untouched_missing_content_is_not_resolved_and_iterator_is_bounded() {
    let tree = [(
        "untouched".into(),
        FileValue {
            content: ContentId::of(b"unavailable"),
            kind: FileKind::Regular,
        },
    )]
    .into();
    let result = apply_edits(
        &tree,
        [],
        |_| -> Result<VerifiedContent, &str> { panic!("untouched content resolved") },
        ApplicationLimits::default(),
    )
    .unwrap();
    assert_eq!(result.tree, tree);
    let (_, edit, _) = fixture(b"old", FileKind::Regular);
    let limits = ApplicationLimits {
        max_edits: 2,
        ..ApplicationLimits::default()
    };
    assert!(matches!(
        apply_edits(
            &Tree::new(),
            std::iter::repeat((edit.id(), edit)),
            |_| Ok::<_, &str>(VerifiedContent::new(Vec::new())),
            limits
        ),
        Err(ApplicationError::TooManyEdits {
            actual: 3,
            limit: 2
        })
    ));
}

#[test]
fn duplicate_ids_and_resolved_budget_fail_before_result() {
    let (tree, edit, blobs) = fixture(b"old", FileKind::Regular);
    assert!(matches!(
        apply_edits(
            &tree,
            [(edit.id(), edit.clone()), (edit.id(), edit.clone())],
            |id| blobs.get(&id).cloned().ok_or("missing"),
            ApplicationLimits::default()
        ),
        Err(ApplicationError::DuplicateEditId(_))
    ));
    let limits = ApplicationLimits {
        max_resolved_bytes: 0,
        ..ApplicationLimits::default()
    };
    assert!(matches!(
        apply_edits(
            &tree,
            [(edit.id(), edit)],
            |id| blobs.get(&id).cloned().ok_or("missing"),
            limits
        ),
        Err(ApplicationError::ByteLimitExceeded {
            generated: false,
            ..
        })
    ));
}

#[test]
fn resolved_byte_limit_is_scoped_to_one_path_group() {
    let first_bytes = b"aaaa".to_vec();
    let second_bytes = b"bbbb".to_vec();
    let first = ContentId::of(&first_bytes);
    let second = ContentId::of(&second_bytes);
    let first_edit = EditRecord::new(
        "a",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: first,
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let second_edit = EditRecord::new(
        "b",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: second,
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let blobs = BTreeMap::from([blob(&first_bytes), blob(&second_bytes)]);
    let limits = ApplicationLimits {
        max_resolved_bytes: 4,
        ..ApplicationLimits::default()
    };

    let result = apply_edits(
        &Tree::new(),
        [
            (first_edit.id(), first_edit),
            (second_edit.id(), second_edit),
        ],
        |id| blobs.get(&id).cloned().ok_or("missing"),
        limits,
    )
    .unwrap();

    assert_eq!(result.tree["a"].content, first);
    assert_eq!(result.tree["b"].content, second);
}

#[test]
fn one_path_group_cannot_exceed_resolved_byte_limit() {
    let bytes = b"oversized".to_vec();
    let content = ContentId::of(&bytes);
    let edit = EditRecord::new(
        "a",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content,
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let limits = ApplicationLimits {
        max_resolved_bytes: bytes.len() - 1,
        ..ApplicationLimits::default()
    };

    assert_eq!(
        apply_edits(
            &Tree::new(),
            [(edit.id(), edit)],
            |id| {
                (id == content)
                    .then(|| VerifiedContent::new(bytes.clone()))
                    .ok_or("missing")
            },
            limits,
        ),
        Err(ApplicationError::ByteLimitExceeded {
            generated: false,
            actual: bytes.len(),
            limit: bytes.len() - 1,
        })
    );
}

#[test]
fn mismatched_edit_id_is_rejected_before_resolution() {
    let (tree, edit, _) = fixture(b"old", FileKind::Regular);
    let supplied = wheat_core::EditId::from_digest([7; 32]);
    assert_ne!(supplied, edit.id());
    assert!(matches!(
        apply_edits(
            &tree,
            [(supplied, edit.clone())],
            |_| -> Result<VerifiedContent, &str> { panic!("mismatched edit resolved content") },
            ApplicationLimits::default(),
        ),
        Err(ApplicationError::EditIdMismatch { supplied: got, actual })
            if got == supplied && actual == edit.id()
    ));
}

#[test]
fn shrinking_changed_output_over_generated_allowance_is_rejected() {
    let (tree, _, mut blobs) = fixture(b"old-tail", FileKind::Regular);
    blobs.extend([blob(b"x")]);
    let edit = EditRecord::new(
        "f",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: ContentId::of(b""),
            expected: ContentId::of(b"old"),
            replacement: ContentId::of(b"x"),
            after: ContentId::of(b""),
            start_anchor: true,
            end_anchor: false,
        },
    )
    .unwrap();
    let limits = ApplicationLimits {
        max_generated_bytes: 2,
        ..ApplicationLimits::default()
    };
    assert_eq!(
        apply_edits(
            &tree,
            [(edit.id(), edit)],
            |id| blobs.get(&id).cloned().ok_or("missing"),
            limits,
        ),
        Err(ApplicationError::ByteLimitExceeded {
            generated: true,
            actual: 6,
            limit: 2,
        })
    );
}
