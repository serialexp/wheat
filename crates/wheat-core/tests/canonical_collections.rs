use wheat_core::*;

#[test]
fn application_nodes_roundtrip_and_enforce_terminal_rules() {
    let value = ApplicationId::from_digest([7; 32]);
    let terminal = ApplicationMapNode::new(vec![7; 16], Some(value), vec![]).unwrap();
    assert_eq!(
        decode_application_map_node(&terminal.canonical_bytes()).unwrap(),
        terminal
    );
    assert!(ApplicationMapNode::new(vec![0; 17], None, vec![]).is_err());
    assert!(ApplicationMapNode::new(vec![], None, vec![]).is_ok());
    let edge = ApplicationMapChild {
        edge: 3,
        child: terminal.id(),
    };
    assert!(ApplicationMapNode::new(vec![0; 16], None, vec![edge]).is_err());
    assert!(ApplicationMapNode::new(vec![], None, vec![edge, edge]).is_err());
}

macro_rules! set_rules {
    ($test:ident, $node:ident, $child:ident, $decode:ident, $tag:expr) => {
        #[test]
        fn $test() {
            let terminal = $node::new(vec![7; 32], true, vec![]).unwrap();
            let bytes = terminal.canonical_bytes();
            assert_eq!($decode(&bytes).unwrap(), terminal);
            assert_eq!(&bytes[..2], &[$tag, 1]);
            assert!($node::new(vec![0; 33], false, vec![]).is_err());
            let edge = $child {
                edge: 3,
                child: terminal.id(),
            };
            assert!($node::new(vec![0; 32], false, vec![edge]).is_err());
            assert!($node::new(vec![], false, vec![edge, edge]).is_err());
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
