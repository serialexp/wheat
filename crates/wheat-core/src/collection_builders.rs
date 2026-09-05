//! Pure, synchronous builders for the canonical fixed-byte radix collections.
//!
//! These are reference builders: inputs are sorted in memory, then nodes are
//! emitted in post-order (children before their parent).  The emitted shape is
//! the same uncompressed, one-node-per-key-byte shape used by `wheat-store`.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ApplicationId, ApplicationMapChild, ApplicationMapId, ApplicationMapNode, CanonicalObject,
    ChangeId, ConflictId, ConflictSetChild, ConflictSetId, ConflictSetNode, ContentId,
    ContentSetChild, ContentSetId, ContentSetNode, ObjectId, RetainedSetChild, RetainedSetId,
    RetainedSetNode, RevisionId, RevisionMapChild, RevisionMapId, RevisionMapNode,
};

trait RadixShape {
    type Key: Ord;
    type Value: Copy;
    type Id: Copy + Into<ObjectId>;

    fn key_bytes(key: &Self::Key) -> &[u8];
    fn node(
        depth: u8,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
    ) -> (Self::Id, Vec<u8>);
}

/// Builds a non-empty subtree from a sorted, key-unique entry slice.
fn build_subtree<S, F>(depth: usize, entries: &[(S::Key, S::Value)], emit: &mut F) -> S::Id
where
    S: RadixShape,
    F: FnMut(CanonicalObject),
{
    debug_assert!(!entries.is_empty());
    let key_len = S::key_bytes(&entries[0].0).len();
    debug_assert!(
        entries
            .iter()
            .all(|(key, _)| S::key_bytes(key).len() == key_len)
    );

    let (value, children) = if depth == key_len {
        debug_assert_eq!(entries.len(), 1);
        (Some(entries[0].1), Vec::new())
    } else {
        let mut children = Vec::new();
        let mut start = 0;
        while start < entries.len() {
            let edge = S::key_bytes(&entries[start].0)[depth];
            let end = start
                + entries[start..].partition_point(|(key, _)| S::key_bytes(key)[depth] == edge);
            let child = build_subtree::<S, F>(depth + 1, &entries[start..end], emit);
            children.push((edge, child));
            start = end;
        }
        (None, children)
    };

    emit_node::<S, F>(depth, value, children, emit)
}

fn emit_node<S, F>(
    depth: usize,
    value: Option<S::Value>,
    children: Vec<(u8, S::Id)>,
    emit: &mut F,
) -> S::Id
where
    S: RadixShape,
    F: FnMut(CanonicalObject),
{
    // Supported keys are 16 or 32 bytes, so recursion depth always fits u8.
    let depth = u8::try_from(depth).expect("canonical radix key length fits in u8");
    let (canonical_id, bytes) = S::node(depth, value, children);
    emit(CanonicalObject {
        id: canonical_id.into(),
        bytes,
    });
    canonical_id
}

fn build_sorted<S, F>(entries: Vec<(S::Key, S::Value)>, mut emit: F) -> S::Id
where
    S: RadixShape,
    F: FnMut(CanonicalObject),
{
    if entries.is_empty() {
        // The root is the sole empty node that is canonical collection output.
        return emit_node::<S, F>(0, None, Vec::new(), &mut emit);
    }
    build_subtree::<S, F>(0, &entries, &mut emit)
}

struct RevisionShape;
impl RadixShape for RevisionShape {
    type Key = ChangeId;
    type Value = RevisionId;
    type Id = RevisionMapId;

    fn key_bytes(key: &Self::Key) -> &[u8] {
        key.as_bytes()
    }

    fn node(
        depth: u8,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = RevisionMapNode::new(
            depth,
            value,
            children
                .into_iter()
                .map(|(edge, child)| RevisionMapChild { edge, child })
                .collect(),
        )
        .expect("builder produces a valid revision-map node");
        let bytes = node.canonical_bytes();
        (RevisionMapId::of_canonical_bytes(&bytes), bytes)
    }
}

struct ApplicationShape;
impl RadixShape for ApplicationShape {
    type Key = ChangeId;
    type Value = ApplicationId;
    type Id = ApplicationMapId;

    fn key_bytes(key: &Self::Key) -> &[u8] {
        key.as_bytes()
    }

    fn node(
        depth: u8,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = ApplicationMapNode::new(
            depth,
            value,
            children
                .into_iter()
                .map(|(edge, child)| ApplicationMapChild { edge, child })
                .collect(),
        )
        .expect("builder produces a valid application-map node");
        let bytes = node.canonical_bytes();
        (ApplicationMapId::of_canonical_bytes(&bytes), bytes)
    }
}

macro_rules! set_shape {
    ($shape:ident, $key:ty, $id:ty, $node:ty, $child:ident) => {
        struct $shape;
        impl RadixShape for $shape {
            type Key = $key;
            type Value = ();
            type Id = $id;

            fn key_bytes(key: &Self::Key) -> &[u8] {
                key.as_bytes()
            }

            fn node(
                depth: u8,
                value: Option<Self::Value>,
                children: Vec<(u8, Self::Id)>,
            ) -> (Self::Id, Vec<u8>) {
                let node = <$node>::new(
                    depth,
                    value.is_some(),
                    children
                        .into_iter()
                        .map(|(edge, child)| $child { edge, child })
                        .collect(),
                )
                .expect("builder produces a valid set node");
                let bytes = node.canonical_bytes();
                (<$id>::of_canonical_bytes(&bytes), bytes)
            }
        }
    };
}

set_shape!(
    ConflictShape,
    ConflictId,
    ConflictSetId,
    ConflictSetNode,
    ConflictSetChild
);
set_shape!(
    ContentShape,
    ContentId,
    ContentSetId,
    ContentSetNode,
    ContentSetChild
);
set_shape!(
    RetainedShape,
    ObjectId,
    RetainedSetId,
    RetainedSetNode,
    RetainedSetChild
);

/// Builds a canonical revision map. If a change occurs more than once, its last
/// revision in iteration order wins.
pub fn build_revision_map(
    entries: impl IntoIterator<Item = (ChangeId, RevisionId)>,
    emit: impl FnMut(CanonicalObject),
) -> RevisionMapId {
    let entries = entries
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    build_sorted::<RevisionShape, _>(entries, emit)
}

/// Builds a canonical application map. If a change occurs more than once, its
/// last application in iteration order wins.
pub fn build_application_map(
    entries: impl IntoIterator<Item = (ChangeId, ApplicationId)>,
    emit: impl FnMut(CanonicalObject),
) -> ApplicationMapId {
    let entries = entries
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    build_sorted::<ApplicationShape, _>(entries, emit)
}

macro_rules! set_builder {
    ($function:ident, $key:ty, $id:ty, $shape:ty) => {
        pub fn $function(
            keys: impl IntoIterator<Item = $key>,
            emit: impl FnMut(CanonicalObject),
        ) -> $id {
            let entries = keys
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|key| (key, ()))
                .collect();
            build_sorted::<$shape, _>(entries, emit)
        }
    };
}

set_builder!(build_conflict_set, ConflictId, ConflictSetId, ConflictShape);
set_builder!(build_content_set, ContentId, ContentSetId, ContentShape);
set_builder!(build_retained_set, ObjectId, RetainedSetId, RetainedShape);
