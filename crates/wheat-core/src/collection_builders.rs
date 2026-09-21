//! Pure, synchronous builders for the canonical fixed-byte radix collections.
//!
//! These are reference builders: inputs are sorted in memory, then maximally
//! path-compressed nodes are emitted in post-order (children before parent).

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ApplicationId, ApplicationMapChild, ApplicationMapId, ApplicationMapNode, CanonicalObject,
    ChangeId, ChangeMapChild, ChangeMapId, ChangeMapNode, ChangeObjectId, ConflictId,
    ConflictSetChild, ConflictSetId, ConflictSetNode, ContentId, ContentSetChild, ContentSetId,
    ContentSetNode, ObjectId, RetainedSetChild, RetainedSetId, RetainedSetNode, RevisionId,
    RevisionMapChild, RevisionMapId, RevisionMapNode, ViewId, ViewLineageMapChild,
    ViewLineageMapId, ViewLineageMapNode,
};

trait RadixShape {
    type Key: Ord;
    type Value: Copy;
    type Id: Copy + Into<ObjectId>;

    fn key_bytes(key: &Self::Key) -> &[u8];
    fn node(
        prefix: Vec<u8>,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<(Vec<u8>, Self::Value)>,
    ) -> (Self::Id, Vec<u8>);
}

/// Builds a non-empty, maximally compressed subtree from sorted unique entries.
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

    let first = S::key_bytes(&entries[0].0);
    let last = S::key_bytes(&entries[entries.len() - 1].0);
    let common = first[depth..]
        .iter()
        .zip(&last[depth..])
        .take_while(|(left, right)| left == right)
        .count();
    let end_prefix = depth + common;
    let prefix = first[depth..end_prefix].to_vec();
    if end_prefix == key_len {
        debug_assert_eq!(entries.len(), 1);
        return emit_node::<S, F>(prefix, Some(entries[0].1), Vec::new(), Vec::new(), emit);
    }

    let mut children = Vec::new();
    let mut inline_entries = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let edge = S::key_bytes(&entries[start].0)[end_prefix];
        let end = start
            + entries[start..]
                .partition_point(|(key, _)| S::key_bytes(key)[end_prefix] == edge);
        if end - start == 1 {
            // Single-entry group: inline it instead of creating a child node.
            let suffix = S::key_bytes(&entries[start].0)[end_prefix..].to_vec();
            inline_entries.push((suffix, entries[start].1));
        } else {
            let child = build_subtree::<S, F>(end_prefix + 1, &entries[start..end], emit);
            children.push((edge, child));
        }
        start = end;
    }

    emit_node::<S, F>(prefix, None, children, inline_entries, emit)
}

fn emit_node<S, F>(
    prefix: Vec<u8>,
    value: Option<S::Value>,
    children: Vec<(u8, S::Id)>,
    entries: Vec<(Vec<u8>, S::Value)>,
    emit: &mut F,
) -> S::Id
where
    S: RadixShape,
    F: FnMut(CanonicalObject),
{
    let (canonical_id, bytes) = S::node(prefix, value, children, entries);
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
        return emit_node::<S, F>(Vec::new(), None, Vec::new(), Vec::new(), &mut emit);
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
        prefix: Vec<u8>,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<(Vec<u8>, Self::Value)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = RevisionMapNode::new(
            prefix,
            value,
            children
                .into_iter()
                .map(|(edge, child)| RevisionMapChild { edge, child })
                .collect(),
            entries,
        )
        .expect("builder produces a valid revision-map node");
        let bytes = node.canonical_bytes();
        (RevisionMapId::of_canonical_bytes(&bytes), bytes)
    }
}

struct ChangeShape;
impl RadixShape for ChangeShape {
    type Key = ChangeId;
    type Value = ChangeObjectId;
    type Id = ChangeMapId;

    fn key_bytes(key: &Self::Key) -> &[u8] {
        key.as_bytes()
    }

    fn node(
        prefix: Vec<u8>,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<(Vec<u8>, Self::Value)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = ChangeMapNode::new(
            prefix,
            value,
            children
                .into_iter()
                .map(|(edge, child)| ChangeMapChild { edge, child })
                .collect(),
            entries,
        )
        .expect("builder produces a valid change-map node");
        let bytes = node.canonical_bytes();
        (ChangeMapId::of_canonical_bytes(&bytes), bytes)
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
        prefix: Vec<u8>,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<(Vec<u8>, Self::Value)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = ApplicationMapNode::new(
            prefix,
            value,
            children
                .into_iter()
                .map(|(edge, child)| ApplicationMapChild { edge, child })
                .collect(),
            entries,
        )
        .expect("builder produces a valid application-map node");
        let bytes = node.canonical_bytes();
        (ApplicationMapId::of_canonical_bytes(&bytes), bytes)
    }
}

struct ViewLineageShape;
impl RadixShape for ViewLineageShape {
    type Key = ViewId;
    type Value = RetainedSetId;
    type Id = ViewLineageMapId;

    fn key_bytes(key: &Self::Key) -> &[u8] {
        key.as_bytes()
    }

    fn node(
        prefix: Vec<u8>,
        value: Option<Self::Value>,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<(Vec<u8>, Self::Value)>,
    ) -> (Self::Id, Vec<u8>) {
        let node = ViewLineageMapNode::new(
            prefix,
            value,
            children
                .into_iter()
                .map(|(edge, child)| ViewLineageMapChild { edge, child })
                .collect(),
            entries,
        )
        .expect("builder produces a valid view-lineage map node");
        let bytes = node.canonical_bytes();
        (ViewLineageMapId::of_canonical_bytes(&bytes), bytes)
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
                prefix: Vec<u8>,
                value: Option<Self::Value>,
                children: Vec<(u8, Self::Id)>,
                _entries: Vec<(Vec<u8>, Self::Value)>,
            ) -> (Self::Id, Vec<u8>) {
                // Set entries: convert (suffix, ()) pairs to just suffixes.
                let set_entries: Vec<Vec<u8>> =
                    _entries.into_iter().map(|(suffix, ())| suffix).collect();
                let node = <$node>::new(
                    prefix,
                    value.is_some(),
                    children
                        .into_iter()
                        .map(|(edge, child)| $child { edge, child })
                        .collect(),
                    set_entries,
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

/// Builds a canonical stable-change lookup map.
pub fn build_change_map(
    entries: impl IntoIterator<Item = (ChangeId, ChangeObjectId)>,
    emit: impl FnMut(CanonicalObject),
) -> ChangeMapId {
    let entries = entries
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    build_sorted::<ChangeShape, _>(entries, emit)
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

/// Builds the canonical parent-view to child-view-set map and emits both object families.
pub fn build_view_lineage_map(
    edges: impl IntoIterator<Item = (ViewId, ViewId)>,
    mut emit: impl FnMut(CanonicalObject),
) -> ViewLineageMapId {
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_unstable();
    edges.dedup();

    let mut entries = Vec::new();
    let mut start = 0;
    while start < edges.len() {
        let parent = edges[start].0;
        let mut end = start + 1;
        while end < edges.len() && edges[end].0 == parent {
            end += 1;
        }
        let child_set = build_retained_set(
            edges[start..end]
                .iter()
                .map(|(_, child)| ObjectId::from(*child)),
            &mut emit,
        );
        entries.push((parent, child_set));
        start = end;
    }
    build_sorted::<ViewLineageShape, _>(entries, emit)
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
