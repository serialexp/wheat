use std::collections::{BTreeMap, BTreeSet};

use wheat_core::{
    AppliedEditEvidence, ByteRange, CanonicalObjectError, ChangeId, ContentId, EditId,
    EditOperation, EditRecord, EditSubsumption, EditTransition, FileKind, RevisionEdits,
    RevisionMetadata, RevisionRecord, TreeId, ViewId, decode_revision_record,
};

fn content(byte: u8) -> ContentId {
    ContentId::from_digest([byte; 32])
}

fn guarded(expected: u8, replacement: u8) -> EditRecord {
    EditRecord::new(
        "src/lib.rs",
        EditOperation::GuardedText {
            kind: FileKind::Regular,
            before: content(1),
            expected: content(expected),
            replacement: content(replacement),
            after: content(4),
            start_anchor: false,
            end_anchor: false,
        },
    )
    .unwrap()
}

#[test]
fn edit_identity_is_independent_of_application_evidence() {
    let edit = guarded(2, 3);
    let first = AppliedEditEvidence {
        target: None,
        read: Some(ByteRange { start: 2, end: 8 }),
        write: Some(ByteRange { start: 3, end: 7 }),
    };
    let second = AppliedEditEvidence {
        target: None,
        read: Some(ByteRange { start: 20, end: 80 }),
        write: None,
    };
    assert_ne!(first, second);
    assert_eq!(edit.id(), edit.id());
    first.validate().unwrap();
    second.validate().unwrap();
}

#[test]
fn guarded_no_op_is_rejected() {
    assert!(matches!(
        EditRecord::new(
            "a",
            EditOperation::GuardedText {
                kind: FileKind::Regular,
                before: content(1),
                expected: content(2),
                replacement: content(2),
                after: content(3),
                start_anchor: false,
                end_anchor: false,
            },
        ),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
}

#[test]
fn transition_accepts_empty_resolution_replacements_and_rejects_bad_subsumption() {
    let edit = guarded(2, 3);
    let id = edit.id();
    let selected = BTreeSet::from([id]);
    let transition = EditTransition {
        added: selected.clone(),
        resolutions: vec![wheat_core::ConflictResolutionEvidence {
            application: wheat_core::ApplicationId::from_digest([9; 32]),
            conflicted_edits: BTreeSet::from([EditId::from_digest([8; 32])]),
            replacement_edits: BTreeSet::new(),
        }],
        ..EditTransition::default()
    };
    transition.validate(&selected).unwrap();

    let bad = RevisionEdits {
        edits: BTreeMap::from([(id, edit)]),
        transition: EditTransition {
            added: selected,
            subsumed: vec![EditSubsumption {
                source: EditId::from_digest([7; 32]),
                replacements: BTreeSet::new(),
            }],
            ..EditTransition::default()
        },
    };
    assert!(bad.validate().is_err());
}

#[test]
fn revision_encodes_selected_ids_not_edit_definitions_in_canonical_order() {
    let first = guarded(2, 3).id();
    let second = guarded(5, 6).id();
    let edits = BTreeSet::from([second, first]);
    let record = RevisionRecord {
        change_id: ChangeId::from_bytes([1; 16]),
        metadata: RevisionMetadata::default(),
        predecessor: None,
        combines: BTreeSet::new(),
        generation: 0,
        dependencies: BTreeSet::new(),
        authored_against: ViewId::from_digest([2; 32]),
        edits: edits.clone(),
        transition: EditTransition {
            added: edits,
            ..EditTransition::default()
        },
        resulting_tree: TreeId::from_digest([3; 32]),
    };
    assert_eq!(
        decode_revision_record(&record.canonical_bytes()).unwrap(),
        record
    );
}

#[test]
fn ranges_are_ordered_and_write_is_contained_in_read() {
    let backwards = AppliedEditEvidence {
        target: None,
        read: Some(ByteRange { start: 9, end: 2 }),
        write: None,
    };
    assert!(backwards.validate().is_err());

    let outside = AppliedEditEvidence {
        target: None,
        read: Some(ByteRange { start: 2, end: 9 }),
        write: Some(ByteRange { start: 1, end: 9 }),
    };
    assert!(outside.validate().is_err());
}
