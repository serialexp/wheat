use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use wheat_core::{
    Application, ApplicationResult, CanonicalObjectError, Conflict, ContentId, DirectoryEntry,
    DirectoryNode, FileEdit, PathConflict, Repository, RepositoryView, RevisionMapChild,
    RevisionMapId, RevisionMapNode, Transformation, Tree, decode_directory_node,
    decode_revision_map_node,
};

fn content(value: &str) -> ContentId {
    ContentId::of(value.as_bytes())
}

fn edit(path: &str, replacement: &str) -> Transformation {
    Transformation {
        edits: vec![FileEdit {
            path: path.into(),
            expected: None,
            replacement: Some(content(replacement)),
        }],
    }
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
        3,
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
    )
    .unwrap();
    let bytes = node.canonical_bytes();

    assert_eq!(&bytes[..3], &[9, 1, 3]);
    assert_eq!(decode_revision_map_node(&bytes).unwrap(), node);
    assert_eq!(node.id(), RevisionMapId::of_canonical_bytes(&bytes));
    assert_eq!(
        hex(&bytes),
        "0901030000000000000002000101010101010101010101010101010101010101010101010101010101010101ff0202020202020202020202020202020202020202020202020202020202020202"
    );
}

#[test]
fn revision_map_nodes_enforce_depth_terminal_and_unique_edges() {
    let revision = wheat_core::RevisionId::from_digest([3; 32]);
    assert!(RevisionMapNode::new(17, None, vec![]).is_err());
    assert!(RevisionMapNode::new(15, Some(revision), vec![]).is_err());
    assert!(
        RevisionMapNode::new(
            16,
            Some(revision),
            vec![RevisionMapChild {
                edge: 0,
                child: RevisionMapId::from_digest([1; 32]),
            }],
        )
        .is_err()
    );
    assert!(
        RevisionMapNode::new(
            0,
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
    let mut first = Repository::new(Tree::new());
    let mut second = first.clone();
    let change = wheat_core::ChangeId::new();

    let first_revision = first
        .create_change_with_id(change, [], edit("file", "value"))
        .unwrap();
    let second_revision = second
        .create_change_with_id(change, [], edit("file", "value"))
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
    let mut repository = Repository::new(Tree::new());
    let change = wheat_core::ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap();
    let outcome = repository
        .create_change_with_id(change, [], edit("file", "value"))
        .unwrap();
    let revision = repository.revision(outcome.revision_id).unwrap();

    assert_eq!(
        hex(&revision.canonical_bytes()),
        "030101890f2d3c4b7a689abcdef012345678000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008b859c7a2265fa9348e3f68d4390528cc45dad0233d59ea3e496f5aa04401a1a000000010000000466696c6500010106444a4916ee6fecd2666eb790b06edd633bb4b58287244579ad6def5fd95e0b7602efbb7d3ed82a46221707b11ee1dc7d7f658ef8ebb5630e2c847ec677d051"
    );
    assert_eq!(
        revision.id().to_string(),
        "965634680afb9da52227beffd0f4e32ea409ba2d286f0db1cd213f65cd816e04"
    );
}

#[test]
fn decoded_repository_rejects_forged_identity_fields() {
    let mut repository = Repository::new(Tree::new());
    let _ = repository.create_change([], edit("file", "value")).unwrap();
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
fn revision_identity_changes_with_encoded_intent() {
    let mut first = Repository::new(Tree::new());
    let mut second = first.clone();
    let change = wheat_core::ChangeId::new();

    let left = first
        .create_change_with_id(change, [], edit("file", "left"))
        .unwrap();
    let right = second
        .create_change_with_id(change, [], edit("file", "right"))
        .unwrap();

    assert_ne!(left.revision_id, right.revision_id);
}

#[test]
fn canonical_map_order_does_not_change_view_identity() {
    let tree = BTreeMap::from([
        ("z".to_owned(), content("last")),
        ("a".to_owned(), content("first")),
    ]);
    let first = RepositoryView::new(tree.clone(), BTreeMap::new());
    let reordered = tree.into_iter().rev().collect::<BTreeMap<_, _>>();
    let second = RepositoryView::new(reordered, BTreeMap::new());

    assert_eq!(first.id, second.id);
}

#[test]
fn conflict_paths_are_canonicalized_before_identity() {
    let view = RepositoryView::new(Tree::new(), BTreeMap::new());
    let a = PathConflict {
        path: "a".into(),
        expected: None,
        actual: None,
        proposed: Some(content("a")),
    };
    let z = PathConflict {
        path: "z".into(),
        expected: None,
        actual: None,
        proposed: Some(content("z")),
    };

    let first = Conflict::new(view.id, vec![z.clone(), a.clone()]);
    let second = Conflict::new(view.id, vec![a, z]);
    assert_eq!(first.id, second.id);
    assert_eq!(first.paths[0].path, "a");
    assert_eq!(
        wheat_core::ConflictId::of_canonical_bytes(&first.canonical_bytes()),
        first.id
    );

    let conflict_id = first.id;
    let _application = Application::new(
        wheat_core::RevisionId::from_digest([7; 32]),
        view.id,
        ApplicationResult::Conflicted {
            partial_tree: Tree::new(),
            conflict: first,
        },
    );
    let manifested = RepositoryView::from_manifest(
        Tree::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeSet::from([conflict_id]),
    );
    assert_ne!(view.id, manifested.id);
}
