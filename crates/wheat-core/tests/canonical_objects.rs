use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use wheat_core::{
    Application, ApplicationContext, ApplicationResult, CanonicalObjectError, Conflict, ContentId,
    ContentIo, DirectoryEntry, DirectoryNode, EditFailure, EditOperation, EditRecord,
    EditTransition, FailedEditEvidence, FileKind, FileValue, Repository, RepositoryView,
    RevisionEdits, RevisionMapChild, RevisionMapId, RevisionMapNode, Tree, ViewOrder,
    decode_directory_node, decode_revision_map_node,
};

fn content(value: &str) -> FileValue {
    file(value, FileKind::Regular)
}

fn file(value: &str, kind: FileKind) -> FileValue {
    FileValue {
        content: ContentId::of(value.as_bytes()),
        kind,
    }
}

#[derive(Default)]
struct TestContent(RefCell<BTreeMap<ContentId, Vec<u8>>>);

impl ContentIo for TestContent {
    type Error = &'static str;

    fn resolve(&self, id: ContentId) -> Result<Vec<u8>, Self::Error> {
        self.0.borrow().get(&id).cloned().ok_or("missing content")
    }

    fn store_generated(&self, id: ContentId, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().insert(id, bytes.to_vec());
        Ok(())
    }
}

fn exact_edit(io: &TestContent, path: &str, replacement: &str, kind: FileKind) -> RevisionEdits {
    let value = file(replacement, kind);
    io.0.borrow_mut()
        .insert(value.content, replacement.as_bytes().to_vec());
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(value),
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

fn edit(io: &TestContent, path: &str, replacement: &str) -> RevisionEdits {
    exact_edit(io, path, replacement, FileKind::Regular)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn directory_nodes_round_trip_with_canonical_component_order() {
    let left = DirectoryNode::new([
        (
            "src".to_owned(),
            DirectoryEntry::Directory(wheat_core::TreeId::from_digest([7; 32])),
        ),
        (
            "README.md".to_owned(),
            DirectoryEntry::File(content("readme")),
        ),
    ])
    .unwrap();
    let right = DirectoryNode::new([
        (
            "README.md".to_owned(),
            DirectoryEntry::File(content("readme")),
        ),
        (
            "src".to_owned(),
            DirectoryEntry::Directory(wheat_core::TreeId::from_digest([7; 32])),
        ),
    ])
    .unwrap();

    assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    assert_eq!(
        decode_directory_node(&left.canonical_bytes()).unwrap(),
        left
    );
    assert_eq!(&left.canonical_bytes()[..2], &[1, 1]);
    assert_eq!(
        left.id(),
        wheat_core::TreeId::of_canonical_bytes(&left.canonical_bytes())
    );
}

#[test]
fn revision_map_nodes_round_trip_with_canonical_edges() {
    let node = RevisionMapNode::new(
        vec![3, 4, 5],
        None,
        vec![
            RevisionMapChild {
                edge: 255,
                child: RevisionMapId::from_digest([2; 32]),
            },
            RevisionMapChild {
                edge: 0,
                child: RevisionMapId::from_digest([1; 32]),
            },
        ],
        vec![],
    )
    .unwrap();
    let bytes = node.canonical_bytes();

    assert_eq!(&bytes[..2], &[9, 1]);
    assert_eq!(decode_revision_map_node(&bytes).unwrap(), node);
    assert_eq!(node.id(), RevisionMapId::of_canonical_bytes(&bytes));
    assert_eq!(RevisionMapId::of_canonical_bytes(&bytes), node.id());
}

#[test]
fn revision_map_nodes_enforce_prefix_terminal_and_unique_edges() {
    let revision = wheat_core::RevisionId::from_digest([3; 32]);
    assert!(RevisionMapNode::new(vec![0; 17], None, vec![], vec![]).is_err());
    assert!(
        RevisionMapNode::new(
            vec![0; 16],
            Some(revision),
            vec![RevisionMapChild {
                edge: 0,
                child: RevisionMapId::from_digest([1; 32]),
            }],
            vec![],
        )
        .is_err()
    );
    assert!(
        RevisionMapNode::new(
            vec![],
            None,
            vec![
                RevisionMapChild {
                    edge: 1,
                    child: RevisionMapId::from_digest([1; 32]),
                },
                RevisionMapChild {
                    edge: 1,
                    child: RevisionMapId::from_digest([2; 32]),
                },
            ],
            vec![],
        )
        .is_err()
    );
}

#[test]
fn directory_nodes_reject_non_components() {
    for invalid in ["", ".", "..", "nested/file", "nul\0name"] {
        assert!(matches!(
            DirectoryNode::new([(invalid.to_owned(), DirectoryEntry::File(content("value")))]),
            Err(CanonicalObjectError::InvalidDirectoryComponent(name)) if name == invalid
        ));
    }
}

#[test]
fn identical_revisions_have_content_identity() {
    let io = TestContent::default();
    let mut first = Repository::new(Tree::new());
    let mut second = first.clone();
    let change = wheat_core::ChangeId::new();

    let first_revision = first
        .create_change_with_id(
            first.base_view().id,
            first.base_view().id,
            change,
            [],
            edit(&io, "file", "value"),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let second_revision = second
        .create_change_with_id(
            second.base_view().id,
            second.base_view().id,
            change,
            [],
            edit(&io, "file", "value"),
            ApplicationContext::new(&io),
        )
        .unwrap();

    assert_eq!(first_revision.revision_id, second_revision.revision_id);
    let revision = first.revision(first_revision.revision_id).unwrap();
    assert_eq!(
        wheat_core::RevisionId::of_canonical_bytes(&revision.canonical_bytes()),
        revision.id()
    );
    assert_eq!(
        wheat_core::ObjectId::from(revision.id()).as_bytes(),
        revision.id().as_bytes()
    );
    assert_eq!(&revision.canonical_bytes()[..2], &[3, 1]);
}

#[test]
fn canonical_revision_has_a_pinned_encoding_and_id() {
    let io = TestContent::default();
    let mut repository = Repository::new(Tree::new());
    let change = wheat_core::ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap();
    let outcome = repository
        .create_change_with_id(
            repository.base_view().id,
            repository.base_view().id,
            change,
            [],
            edit(&io, "file", "value"),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let revision = repository.revision(outcome.revision_id).unwrap();

    assert_eq!(
        hex(&revision.canonical_bytes()),
        "030101890f2d3c4b7a689abcdef0123456780000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000048024dc114434a502c17cfc9fa047f0515a932ecfd4b8b91c13fcd5d15debf3600000001c5a1e43cf710947af7add28e119fc2713b16f4f292c70d619a25acbc0ee95ff60000000000000001c5a1e43cf710947af7add28e119fc2713b16f4f292c70d619a25acbc0ee95ff6000000000000000000000000698e871a850b91550b4b566205c4f93d956bd143e8c5a3828829d31ab5fa89a6"
    );
    // Revision ID is BLAKE3 of the canonical bytes above.
    let expected_id = wheat_core::RevisionId::of_canonical_bytes(&revision.canonical_bytes());
    assert_eq!(revision.id(), expected_id);
}

#[test]
fn revision_parents_and_view_order_are_checked_on_decode() {
    use wheat_core::{
        ChangeId, RevisionId, RevisionRecord, TreeId, ViewId, ViewRecord, decode_revision_record,
        decode_view_record,
    };
    let predecessor = RevisionId::from_digest([1; 32]);
    let other = RevisionId::from_digest([2; 32]);
    let revision = RevisionRecord {
        change_id: ChangeId::from_bytes([1; 16]),
        metadata: wheat_core::RevisionMetadata::default(),
        predecessor: Some(predecessor),
        combines: BTreeSet::from([other]),
        generation: 3,
        dependencies: BTreeSet::new(),
        authored_against: ViewId::from_digest([3; 32]),
        edits: BTreeSet::new(),
        transition: EditTransition::default(),
        resulting_tree: TreeId::from_digest([4; 32]),
    };
    assert_eq!(
        decode_revision_record(&revision.canonical_bytes()).unwrap(),
        revision
    );
    let refused =
        |forged: RevisionRecord| decode_revision_record(&forged.canonical_bytes()).is_err();
    // A combined revision never combines its own predecessor.
    assert!(refused(RevisionRecord {
        combines: BTreeSet::from([predecessor, other]),
        ..revision.clone()
    }));
    // Combining needs a predecessor, and only a revision without one has generation 0.
    assert!(refused(RevisionRecord {
        predecessor: None,
        ..revision.clone()
    }));
    assert!(refused(RevisionRecord {
        generation: 0,
        ..revision.clone()
    }));
    assert!(refused(RevisionRecord {
        predecessor: None,
        combines: BTreeSet::new(),
        generation: 1,
        ..revision.clone()
    }));
    let too_many = (0..=wheat_core::MAX_COMBINES)
        .map(|index| RevisionId::from_digest([u8::try_from(index + 10).unwrap(); 32]))
        .collect();
    assert!(refused(RevisionRecord {
        combines: too_many,
        ..revision.clone()
    }));

    let base = ViewRecord {
        tree: TreeId::from_digest([5; 32]),
        revisions: RevisionMapId::from_digest([6; 32]),
        applications: wheat_core::ApplicationMapId::from_digest([7; 32]),
        unresolved_conflicts: wheat_core::ConflictSetId::from_digest([8; 32]),
        required_content: wheat_core::ContentMapId::from_digest([9; 32]),
        order: ViewOrder::BASE,
    };
    let change = ChangeId::from_bytes([2; 16]);
    let one = ViewOrder::BASE.then(change, 4).unwrap();
    assert_eq!(
        one,
        ViewOrder {
            size: 1,
            last: Some(change),
            weight: 5
        }
    );
    for order in [ViewOrder::BASE, one] {
        let view = ViewRecord { order, ..base };
        assert_eq!(decode_view_record(&view.canonical_bytes()).unwrap(), view);
    }
    for order in [
        ViewOrder {
            last: Some(change),
            ..ViewOrder::BASE
        },
        ViewOrder {
            weight: 1,
            ..ViewOrder::BASE
        },
        ViewOrder { last: None, ..one },
        ViewOrder { weight: 0, ..one },
    ] {
        // View encoding runs the canonical check decoding runs, so a forged order
        // cannot even be encoded.
        let view = ViewRecord { order, ..base };
        assert!(
            std::panic::catch_unwind(move || view.canonical_bytes()).is_err(),
            "{order:?}"
        );
    }
    assert_eq!(ViewOrder::BASE.then(change, u64::MAX), None);
}

#[test]
fn decoded_repository_rejects_forged_identity_fields() {
    let io = TestContent::default();
    let mut repository = Repository::new(Tree::new());
    let _ = repository
        .create_change(
            repository.base_view().id,
            repository.base_view().id,
            [],
            edit(&io, "file", "value"),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let mut value = serde_yaml::to_value(&repository).unwrap();
    let revisions = value
        .get_mut("revisions")
        .and_then(serde_yaml::Value::as_mapping_mut)
        .unwrap();
    let revision = revisions.values_mut().next().unwrap();
    revision["id"] = serde_yaml::to_value(wheat_core::RevisionId::from_digest([0; 32])).unwrap();

    let decoded: Repository = serde_yaml::from_value(value).unwrap();
    assert!(decoded.validate().is_err());
}

#[test]
fn file_kind_changes_tree_and_revision_identity() {
    let regular = RepositoryView::new(
        BTreeMap::from([("file".to_owned(), file("same", FileKind::Regular))]),
        BTreeMap::new(),
        ViewOrder::BASE,
    );
    let executable = RepositoryView::new(
        BTreeMap::from([("file".to_owned(), file("same", FileKind::Executable))]),
        BTreeMap::new(),
        ViewOrder::BASE,
    );
    let symlink = RepositoryView::new(
        BTreeMap::from([("file".to_owned(), file("same", FileKind::Symlink))]),
        BTreeMap::new(),
        ViewOrder::BASE,
    );
    assert_ne!(regular.id, executable.id);
    assert_ne!(regular.id, symlink.id);
    assert_ne!(executable.id, symlink.id);

    let io = TestContent::default();
    let change = wheat_core::ChangeId::new();
    let mut first = Repository::new(Tree::new());
    let mut second = first.clone();
    let regular_revision = first
        .create_change_with_id(
            first.base_view().id,
            first.base_view().id,
            change,
            [],
            exact_edit(&io, "file", "same", FileKind::Regular),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let executable_revision = second
        .create_change_with_id(
            second.base_view().id,
            second.base_view().id,
            change,
            [],
            exact_edit(&io, "file", "same", FileKind::Executable),
            ApplicationContext::new(&io),
        )
        .unwrap();
    assert_ne!(
        regular_revision.revision_id,
        executable_revision.revision_id
    );
}

#[test]
fn revision_identity_changes_with_encoded_intent() {
    let io = TestContent::default();
    let mut first = Repository::new(Tree::new());
    let mut second = first.clone();
    let change = wheat_core::ChangeId::new();

    let left = first
        .create_change_with_id(
            first.base_view().id,
            first.base_view().id,
            change,
            [],
            edit(&io, "file", "left"),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let right = second
        .create_change_with_id(
            second.base_view().id,
            second.base_view().id,
            change,
            [],
            edit(&io, "file", "right"),
            ApplicationContext::new(&io),
        )
        .unwrap();

    assert_ne!(left.revision_id, right.revision_id);
}

#[test]
fn canonical_map_order_does_not_change_view_identity() {
    let tree = BTreeMap::from([
        ("z".to_owned(), content("last")),
        ("a".to_owned(), content("first")),
    ]);
    let first = RepositoryView::new(tree.clone(), BTreeMap::new(), ViewOrder::BASE);
    let reordered = tree.into_iter().rev().collect::<BTreeMap<_, _>>();
    let second = RepositoryView::new(reordered, BTreeMap::new(), ViewOrder::BASE);

    assert_eq!(first.id, second.id);
}

#[test]
fn conflict_edits_are_canonicalized_before_identity() {
    let view = RepositoryView::new(Tree::new(), BTreeMap::new(), ViewOrder::BASE);
    let a = EditRecord::new(
        "a",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(content("a")),
        },
    )
    .unwrap()
    .id();
    let z = EditRecord::new(
        "z",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(content("z")),
        },
    )
    .unwrap()
    .id();
    let failure = FailedEditEvidence {
        target: None,
        reason: EditFailure::ExpectedFileMismatch,
    };

    let first = Conflict::new(
        view.id,
        [(z, failure.clone()), (a, failure.clone())]
            .into_iter()
            .collect(),
    );
    let second = Conflict::new(
        view.id,
        [(a, failure.clone()), (z, failure)].into_iter().collect(),
    );
    assert_eq!(first.id, second.id);
    assert_eq!(first.edits, second.edits);
    assert_eq!(
        wheat_core::ConflictId::of_canonical_bytes(&first.canonical_bytes()),
        first.id
    );

    let conflict_id = first.id;
    let _application = Application::new(
        wheat_core::RevisionId::from_digest([7; 32]),
        view.id,
        wheat_core::ApplicationOrder::begin(wheat_core::ChangeId::from_bytes([7; 16]), 0, None),
        ApplicationResult::Conflicted {
            partial_tree: Tree::new(),
            conflict: first,
        },
        BTreeMap::new(),
    );
    let manifested = RepositoryView::from_manifest(
        Tree::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeSet::from([conflict_id]),
        ViewOrder::BASE,
    );
    assert_ne!(view.id, manifested.id);
}
