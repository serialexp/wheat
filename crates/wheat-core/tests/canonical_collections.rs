use wheat_core::*;

#[test]
fn change_nodes_roundtrip_and_enforce_terminal_rules() {
    let value = ChangeObjectId::from_digest([7; 32]);
    let terminal = ChangeMapNode::new(vec![7; 16], Some(value), vec![], vec![]).unwrap();
    assert_eq!(
        decode_change_map_node(&terminal.canonical_bytes()).unwrap(),
        terminal
    );
    assert!(ChangeMapNode::new(vec![0; 17], None, vec![], vec![]).is_err());
    assert!(ChangeMapNode::new(vec![], None, vec![], vec![]).is_ok());
    let edge = ChangeMapChild {
        edge: 3,
        child: terminal.id(),
    };
    assert!(ChangeMapNode::new(vec![0; 16], None, vec![edge], vec![]).is_err());
    assert!(ChangeMapNode::new(vec![], None, vec![edge, edge], vec![]).is_err());
}

#[test]
fn application_nodes_roundtrip_and_enforce_terminal_rules() {
    let value = ApplicationId::from_digest([7; 32]);
    let terminal = ApplicationMapNode::new(vec![7; 16], Some(value), vec![], vec![]).unwrap();
    assert_eq!(
        decode_application_map_node(&terminal.canonical_bytes()).unwrap(),
        terminal
    );
    assert!(ApplicationMapNode::new(vec![0; 17], None, vec![], vec![]).is_err());
    assert!(ApplicationMapNode::new(vec![], None, vec![], vec![]).is_ok());
    let edge = ApplicationMapChild {
        edge: 3,
        child: terminal.id(),
    };
    assert!(ApplicationMapNode::new(vec![0; 16], None, vec![edge], vec![]).is_err());
    assert!(ApplicationMapNode::new(vec![], None, vec![edge, edge], vec![]).is_err());
}

#[test]
fn view_lineage_nodes_roundtrip_and_enforce_terminal_rules() {
    let value = RetainedSetId::from_digest([7; 32]);
    let terminal = ViewLineageMapNode::new(vec![7; 32], Some(value), vec![], vec![]).unwrap();
    let bytes = terminal.canonical_bytes();
    assert_eq!(decode_view_lineage_map_node(&bytes).unwrap(), terminal);
    assert_eq!(&bytes[..2], &[18, 1]);
    assert!(ViewLineageMapNode::new(vec![0; 33], None, vec![], vec![]).is_err());
    assert!(ViewLineageMapNode::new(vec![], None, vec![], vec![]).is_ok());
    let edge = ViewLineageMapChild {
        edge: 3,
        child: terminal.id(),
    };
    assert!(ViewLineageMapNode::new(vec![0; 32], None, vec![edge], vec![]).is_err());
    assert!(ViewLineageMapNode::new(vec![], None, vec![edge, edge], vec![]).is_err());
    assert!(decode_application_map_node(&bytes).is_err());
}

#[test]
fn view_lineage_builder_groups_edges_and_emits_child_sets() {
    let parent = ViewId::from_digest([1; 32]);
    let first = ViewId::from_digest([2; 32]);
    let second = ViewId::from_digest([3; 32]);
    let mut objects = Vec::new();
    let root = build_view_lineage_map(
        [(parent, second), (parent, first), (parent, first)],
        |object| objects.push(object),
    );
    let map = objects
        .iter()
        .find_map(|object| decode_view_lineage_map_node(&object.bytes).ok())
        .unwrap();
    assert_eq!(map.id(), root);
    let child_root = map.value.unwrap();
    let retained_nodes: Vec<_> = objects
        .iter()
        .filter_map(|object| decode_retained_set_node(&object.bytes).ok())
        .collect();
    assert!(retained_nodes.iter().any(|node| node.id() == child_root));

    let mut reordered = Vec::new();
    assert_eq!(
        build_view_lineage_map([(parent, first), (parent, second)], |object| {
            reordered.push(object)
        }),
        root
    );
    assert_eq!(objects, reordered);
}

macro_rules! set_rules {
    ($test:ident, $node:ident, $child:ident, $decode:ident, $tag:expr) => {
        #[test]
        fn $test() {
            let terminal = $node::new(vec![7; 32], true, vec![], vec![]).unwrap();
            let bytes = terminal.canonical_bytes();
            assert_eq!($decode(&bytes).unwrap(), terminal);
            assert_eq!(&bytes[..2], &[$tag, 1]);
            assert!($node::new(vec![0; 33], false, vec![], vec![]).is_err());
            let edge = $child {
                edge: 3,
                child: terminal.id(),
            };
            assert!($node::new(vec![0; 32], false, vec![edge], vec![]).is_err());
            assert!($node::new(vec![], false, vec![edge, edge], vec![]).is_err());
            assert!(decode_revision_map_node(&bytes).is_err());
            let mut trailing = bytes;
            trailing.push(0);
            assert!($decode(&trailing).is_err());
        }
    };
}
set_rules!(
    conflict_set_rules,
    ConflictSetNode,
    ConflictSetChild,
    decode_conflict_set_node,
    11
);
set_rules!(
    content_set_rules,
    ContentSetNode,
    ContentSetChild,
    decode_content_set_node,
    12
);
set_rules!(
    retained_set_rules,
    RetainedSetNode,
    RetainedSetChild,
    decode_retained_set_node,
    13
);
