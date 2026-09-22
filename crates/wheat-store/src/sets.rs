use thiserror::Error;
use wheat_core::{
    ConflictId, ConflictSetChild, ConflictSetId, ConflictSetNode, ContentId, ContentSetId,
    ObjectId, RetainedSetChild, RetainedSetId, RetainedSetNode, decode_conflict_set_node,
    decode_retained_set_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

/// One authenticated membership difference between two content manifests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSetDelta {
    Added(ContentId),
    Removed(ContentId),
}

/// One authenticated membership difference between two retained-object sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedSetDelta {
    Added(ObjectId),
    Removed(ObjectId),
}

/// Bounds for a read-only content-manifest comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentSetDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for ContentSetDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
        }
    }
}

/// Bounds for a read-only retained-set comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedSetDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for RetainedSetDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
        }
    }
}

#[derive(Debug, Error)]
pub enum CollectionError {
    #[error("immutable collection storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("collection node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("collection root must have depth zero")]
    InvalidRootDepth,
    #[error("collection child depth does not match its parent")]
    InvalidChildDepth,
    #[error("collection values are permitted only at terminal depth")]
    InvalidTerminalNode,
    #[error("set delta contains more than {limit} entries")]
    DeltaChangeLimitExceeded { limit: usize },
    #[error("set delta reads more than {limit} nodes")]
    DeltaNodeLimitExceeded { limit: usize },
    #[error("set delta queues more than {limit} pending subtrees")]
    DeltaPendingLimitExceeded { limit: usize },
}
impl From<radix::Error> for CollectionError {
    fn from(e: radix::Error) -> Self {
        match e {
            radix::Error::Store(e) => Self::Store(e),
            radix::Error::Decode(e) => Self::Decode(e),
            radix::Error::InvalidRootDepth => Self::InvalidRootDepth,
            radix::Error::InvalidChildDepth => Self::InvalidChildDepth,
            radix::Error::InvalidTerminalNode => Self::InvalidTerminalNode,
        }
    }
}
impl From<radix::DiffError> for CollectionError {
    fn from(error: radix::DiffError) -> Self {
        match error {
            radix::DiffError::Radix(error) => error.into(),
            radix::DiffError::ChangeLimitExceeded { limit } => {
                Self::DeltaChangeLimitExceeded { limit }
            }
            radix::DiffError::NodeLimitExceeded { limit } => Self::DeltaNodeLimitExceeded { limit },
            radix::DiffError::PendingLimitExceeded { limit } => {
                Self::DeltaPendingLimitExceeded { limit }
            }
        }
    }
}

macro_rules! digest_key {
    ($key:ty) => {
        impl radix::Key for $key {
            const LEN: usize = 32;
            fn bytes(&self) -> &[u8] {
                self.as_bytes()
            }
            fn from_bytes(bytes: &[u8]) -> Self {
                Self::from_digest(bytes.try_into().expect("radix key length is fixed"))
            }
        }
    };
}
digest_key!(ConflictId);
digest_key!(ContentId);
digest_key!(ObjectId);

macro_rules! set_collection {
    ($collection:ident,$id:ty,$key:ty,$node:ty,$child:ident,$kind:ident,$decode:ident) => {
        pub(crate) struct $collection;
        impl radix::Collection for $collection {
            type Id = $id;
            type Key = $key;
            type Value = ();
            type PublicNode = $node;
            const KIND: ObjectKind = ObjectKind::$kind;
            fn decode(b: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
                $decode(b)
            }
            fn unpack(n: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
                radix::Node {
                    prefix: n.prefix,
                    value: n.present.then_some(()),
                    children: n.children.into_iter().map(|c| (c.edge, c.child)).collect(),
                    entries: n.entries.into_iter().map(|suffix| (suffix, ())).collect(),
                }
            }
            fn build(
                n: radix::Node<Self::Id, Self::Value>,
            ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
                <$node>::new(
                    n.prefix,
                    n.value.is_some(),
                    n.children
                        .into_iter()
                        .map(|(edge, child)| $child { edge, child })
                        .collect(),
                    n.entries.into_iter().map(|(suffix, ())| suffix).collect(),
                )
            }
            fn id(n: &Self::PublicNode) -> Self::Id {
                n.id()
            }
            fn canonical_bytes(n: &Self::PublicNode) -> Vec<u8> {
                n.canonical_bytes()
            }
        }
    };
}
set_collection!(
    ConflictCollection,
    ConflictSetId,
    ConflictId,
    ConflictSetNode,
    ConflictSetChild,
    ConflictSetNode,
    decode_conflict_set_node
);
// ContentCollection (the radix-trie bridge) has been removed. Content sets
// are now stored as flat sorted snapshot/delta blobs — see ContentSetStore above.
set_collection!(
    RetainedCollection,
    RetainedSetId,
    ObjectId,
    RetainedSetNode,
    RetainedSetChild,
    RetainedSetNode,
    decode_retained_set_node
);

macro_rules! set_api {
    ($store:ident,$cursor:ident,$collection:ident,$id:ty,$key:ty,$node:ty) => {
        pub struct $store<'a, S> {
            inner: radix::Store<'a, S, $collection>,
        }
        pub struct $cursor<'a, S> {
            inner: radix::Cursor<'a, S, $collection>,
        }
        impl<S: ImmutableObjectStore> $cursor<'_, S> {
            pub async fn next(&mut self) -> Result<Option<$key>, CollectionError> {
                Ok(self.inner.next().await?.map(|(key, ())| key))
            }
        }
        impl<'a, S: ImmutableObjectStore> $store<'a, S> {
            #[must_use]
            pub fn new(objects: &'a S) -> Self {
                Self {
                    inner: radix::Store::new(objects),
                }
            }
            pub async fn empty_root(&self) -> Result<$id, CollectionError> {
                Ok(self.inner.empty_root().await?)
            }
            pub async fn read_node(&self, id: $id) -> Result<$node, CollectionError> {
                Ok(self.inner.read_public(id).await?)
            }
            /// Read and contextually validate a node reached at the given key position.
            pub async fn read_node_at(
                &self,
                id: $id,
                position: usize,
            ) -> Result<$node, CollectionError> {
                Ok(self.inner.read_public_at(id, position).await?)
            }
            #[allow(dead_code)] // Only retained sets currently need byte-accounted lineage reads.
            pub(crate) async fn read_node_at_with_size(
                &self,
                id: $id,
                position: usize,
            ) -> Result<($node, usize), CollectionError> {
                Ok(self.inner.read_public_at_with_size(id, position).await?)
            }
            /// Persist one already-canonical node produced by a sorted collection builder.
            pub async fn write_node(&self, node: $node) -> Result<$id, CollectionError> {
                let id = node.id();
                self.inner.write_public(id, node.canonical_bytes()).await?;
                Ok(id)
            }
            pub async fn contains_node(&self, id: $id) -> Result<bool, CollectionError> {
                Ok(self.inner.contains_node(id).await?)
            }
            pub async fn contains(&self, root: $id, key: $key) -> Result<bool, CollectionError> {
                Ok(self.inner.get(root, key).await?.is_some())
            }
            pub async fn insert(&self, root: $id, key: $key) -> Result<$id, CollectionError> {
                self.update_batch(root, [(key, true)]).await
            }
            pub async fn remove(&self, root: $id, key: $key) -> Result<$id, CollectionError> {
                self.update_batch(root, [(key, false)]).await
            }
            pub async fn update_batch(
                &self,
                root: $id,
                edits: impl IntoIterator<Item = ($key, bool)>,
            ) -> Result<$id, CollectionError> {
                Ok(self
                    .inner
                    .update_batch(root, edits.into_iter().map(|(k, p)| (k, p.then_some(()))))
                    .await?)
            }
            pub async fn cursor(&self, root: $id) -> Result<$cursor<'a, S>, CollectionError> {
                Ok($cursor {
                    inner: self.inner.cursor(root).await?,
                })
            }
            /// Returns the authenticated structural union of two roots.
            ///
            /// Equal and disjoint subtrees are reused directly; only divergent ancestors are
            /// emitted, so memory is bounded by the radix depth and fanout.
            pub async fn union(&self, root: $id, other: $id) -> Result<$id, CollectionError> {
                Ok(self.inner.union(root, other).await?)
            }
            /// Removes all keys in `other`, retaining only a bounded edit chunk in memory.
            pub async fn difference(
                &self,
                mut root: $id,
                other: $id,
            ) -> Result<$id, CollectionError> {
                root = self.update_batch(root, []).await?;
                let mut cursor = self.cursor(other).await?;
                let mut chunk = Vec::with_capacity(1024);
                loop {
                    while chunk.len() < 1024 {
                        match cursor.next().await? {
                            Some(k) => chunk.push((k, false)),
                            None => break,
                        }
                    }
                    if chunk.is_empty() {
                        return Ok(root);
                    }
                    root = self.update_batch(root, chunk.drain(..)).await?;
                }
            }
        }
    };
}
set_api!(
    ConflictSetStore,
    ConflictSetCursor,
    ConflictCollection,
    ConflictSetId,
    ConflictId,
    ConflictSetNode
);
/// Maximum delta chain depth before a full snapshot is forced.
const CONTENT_SET_CHAIN_DEPTH: u32 = 16;

/// Flat sorted blob content set store.
///
/// Replaces the radix trie implementation with flat sorted arrays of `ContentId`
/// values. Full snapshots are written every 16 views (chain depth 16); intermediate
/// views store compact deltas referencing the previous view's content set.
pub struct ContentSetStore<'a, S> {
    objects: &'a S,
}

/// Iterator over the resolved members of a content set.
pub struct ContentSetCursor {
    members: Vec<ContentId>,
    index: usize,
}

impl ContentSetCursor {
    pub async fn next(&mut self) -> Result<Option<ContentId>, CollectionError> {
        if self.index < self.members.len() {
            let id = self.members[self.index];
            self.index += 1;
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }
}

impl<'a, S: ImmutableObjectStore> ContentSetStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self { objects }
    }

    /// Resolve a `ContentSetId` to its full sorted membership by walking the
    /// delta chain (at most `CONTENT_SET_CHAIN_DEPTH` reads).
    pub fn resolve(
        &self,
        root: ContentSetId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<ContentId>, CollectionError>> + Send + '_>,
    > {
        Box::pin(async move {
            let stored = self.objects.get(root.into()).await?;
            let blob = wheat_core::decode_content_set_blob(&stored.canonical_bytes)?;
            match blob {
                wheat_core::ContentSetBlob::Snapshot(members) => Ok(members),
                wheat_core::ContentSetBlob::Delta {
                    base,
                    added,
                    removed,
                } => {
                    let mut base_members = self.resolve(base).await?;
                    apply_delta(&mut base_members, &added, &removed);
                    Ok(base_members)
                }
            }
        })
    }

    /// Create an empty content set (snapshot with zero members).
    pub async fn empty_root(&self) -> Result<ContentSetId, CollectionError> {
        self.write_snapshot(&[]).await
    }

    /// Write a full snapshot blob.
    pub async fn write_snapshot(
        &self,
        members: &[ContentId],
    ) -> Result<ContentSetId, CollectionError> {
        let bytes = wheat_core::content_set_snapshot_bytes(members);
        let id = ContentSetId::of_canonical_bytes(&bytes);
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(id)
    }

    /// Write a delta blob referencing a base content set.
    pub async fn write_delta(
        &self,
        base: ContentSetId,
        added: &[ContentId],
        removed: &[ContentId],
    ) -> Result<ContentSetId, CollectionError> {
        let bytes = wheat_core::content_set_delta_bytes(base, added, removed);
        let id = ContentSetId::of_canonical_bytes(&bytes);
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(id)
    }

    /// Check membership of a single key.
    pub async fn contains(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<bool, CollectionError> {
        let members = self.resolve(root).await?;
        Ok(members.binary_search(&key).is_ok())
    }

    /// Insert a single key, producing a snapshot.
    pub async fn insert(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch(root, [(key, true)]).await
    }

    /// Remove a single key, producing a snapshot.
    pub async fn remove(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch(root, [(key, false)]).await
    }

    /// Apply a batch of inserts/removals, always producing a new snapshot.
    ///
    /// Use `update_with_depth` for chain-depth-aware delta/snapshot selection.
    pub async fn update_batch(
        &self,
        root: ContentSetId,
        edits: impl IntoIterator<Item = (ContentId, bool)>,
    ) -> Result<ContentSetId, CollectionError> {
        let mut members = self.resolve(root).await?;
        let old_members = members.clone();
        apply_edits(&mut members, edits);
        if members == old_members {
            return Ok(root);
        }
        self.write_snapshot(&members).await
    }

    /// Apply edits with chain-depth awareness: produces a delta if within the
    /// chain depth limit, otherwise a full snapshot.
    ///
    /// Returns `(new_root, new_depth)`. The depth should be stored alongside
    /// the content set reference for subsequent updates.
    pub async fn update_with_depth(
        &self,
        root: ContentSetId,
        edits: impl IntoIterator<Item = (ContentId, bool)>,
        parent_depth: u32,
    ) -> Result<(ContentSetId, u32), CollectionError> {
        let mut members = self.resolve(root).await?;
        let old_members = members.clone();
        apply_edits(&mut members, edits);

        if members == old_members {
            return Ok((root, parent_depth));
        }

        let new_depth = parent_depth + 1;
        if new_depth.is_multiple_of(CONTENT_SET_CHAIN_DEPTH) {
            // Chain depth boundary: write a full snapshot.
            let id = self.write_snapshot(&members).await?;
            Ok((id, new_depth))
        } else {
            // Within chain: write a delta.
            let (added, removed) = compute_delta(&old_members, &members);
            if added.is_empty() && removed.is_empty() {
                Ok((root, parent_depth))
            } else {
                let id = self.write_delta(root, &added, &removed).await?;
                Ok((id, new_depth))
            }
        }
    }

    /// Iterate all members in sorted order.
    pub async fn cursor(&self, root: ContentSetId) -> Result<ContentSetCursor, CollectionError> {
        let members = self.resolve(root).await?;
        Ok(ContentSetCursor { members, index: 0 })
    }

    /// Structural union of two content sets, producing a snapshot.
    pub async fn union(
        &self,
        root: ContentSetId,
        other: ContentSetId,
    ) -> Result<ContentSetId, CollectionError> {
        if root == other {
            return Ok(root);
        }
        let a = self.resolve(root).await?;
        let b = self.resolve(other).await?;
        let merged = sorted_union(&a, &b);
        self.write_snapshot(&merged).await
    }

    /// Difference: `root` minus `other`, producing a snapshot.
    pub async fn difference(
        &self,
        root: ContentSetId,
        other: ContentSetId,
    ) -> Result<ContentSetId, CollectionError> {
        if root == other {
            return self.empty_root().await;
        }
        let a = self.resolve(root).await?;
        let b = self.resolve(other).await?;
        let diff = sorted_difference(&a, &b);
        self.write_snapshot(&diff).await
    }

    /// Compare two content sets and return the added/removed members.
    pub async fn delta(
        &self,
        old: ContentSetId,
        new: ContentSetId,
        _limits: ContentSetDeltaLimits,
    ) -> Result<Vec<ContentSetDelta>, CollectionError> {
        if old == new {
            return Ok(Vec::new());
        }
        let old_members = self.resolve(old).await?;
        let new_members = self.resolve(new).await?;
        let mut result = Vec::new();
        let mut oi = 0;
        let mut ni = 0;
        while oi < old_members.len() && ni < new_members.len() {
            match old_members[oi].cmp(&new_members[ni]) {
                std::cmp::Ordering::Less => {
                    result.push(ContentSetDelta::Removed(old_members[oi]));
                    oi += 1;
                }
                std::cmp::Ordering::Greater => {
                    result.push(ContentSetDelta::Added(new_members[ni]));
                    ni += 1;
                }
                std::cmp::Ordering::Equal => {
                    oi += 1;
                    ni += 1;
                }
            }
        }
        for &id in &old_members[oi..] {
            result.push(ContentSetDelta::Removed(id));
        }
        for &id in &new_members[ni..] {
            result.push(ContentSetDelta::Added(id));
        }
        Ok(result)
    }

    /// Derive the chain depth of a content set by walking the delta chain.
    ///
    /// Returns 0 for snapshots, 1 + base_depth for deltas.
    pub fn chain_depth(
        &self,
        root: ContentSetId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u32, CollectionError>> + Send + '_>,
    > {
        Box::pin(async move {
            let stored = self.objects.get(root.into()).await?;
            let blob = wheat_core::decode_content_set_blob(&stored.canonical_bytes)?;
            match blob {
                wheat_core::ContentSetBlob::Snapshot(_) => Ok(0),
                wheat_core::ContentSetBlob::Delta { base, .. } => {
                    Ok(1 + self.chain_depth(base).await?)
                }
            }
        })
    }
}

/// Apply a delta (added/removed) to a sorted membership vector.
fn apply_delta(members: &mut Vec<ContentId>, added: &[ContentId], removed: &[ContentId]) {
    // Remove first, then add — both arrays are sorted.
    if !removed.is_empty() {
        members.retain(|id| removed.binary_search(id).is_err());
    }
    if !added.is_empty() {
        // Merge-insert added entries into the sorted vec.
        let mut result = Vec::with_capacity(members.len() + added.len());
        let mut mi = 0;
        let mut ai = 0;
        while mi < members.len() && ai < added.len() {
            match members[mi].cmp(&added[ai]) {
                std::cmp::Ordering::Less => {
                    result.push(members[mi]);
                    mi += 1;
                }
                std::cmp::Ordering::Greater => {
                    result.push(added[ai]);
                    ai += 1;
                }
                std::cmp::Ordering::Equal => {
                    result.push(members[mi]);
                    mi += 1;
                    ai += 1;
                }
            }
        }
        result.extend_from_slice(&members[mi..]);
        result.extend_from_slice(&added[ai..]);
        *members = result;
    }
}

/// Apply a batch of edits (key, insert=true/remove=false) to a sorted membership vector.
///
/// Last-edit-wins: if the same key appears multiple times, the final operation
/// in iteration order determines whether the key is present or absent.
fn apply_edits(members: &mut Vec<ContentId>, edits: impl IntoIterator<Item = (ContentId, bool)>) {
    // Collect last-wins state per key.
    let mut final_state = std::collections::BTreeMap::new();
    for (key, insert) in edits {
        final_state.insert(key, insert);
    }
    let mut adds = Vec::new();
    let mut removes = Vec::new();
    for (key, insert) in final_state {
        if insert {
            adds.push(key);
        } else {
            removes.push(key);
        }
    }
    // BTreeMap iteration is already sorted; no need to sort again.
    apply_delta(members, &adds, &removes);
}

/// Compute the delta between two sorted membership arrays.
fn compute_delta(old: &[ContentId], new: &[ContentId]) -> (Vec<ContentId>, Vec<ContentId>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut oi = 0;
    let mut ni = 0;
    while oi < old.len() && ni < new.len() {
        match old[oi].cmp(&new[ni]) {
            std::cmp::Ordering::Less => {
                removed.push(old[oi]);
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(new[ni]);
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                oi += 1;
                ni += 1;
            }
        }
    }
    removed.extend_from_slice(&old[oi..]);
    added.extend_from_slice(&new[ni..]);
    (added, removed)
}

/// Sorted union of two sorted, unique arrays.
fn sorted_union(a: &[ContentId], b: &[ContentId]) -> Vec<ContentId> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let mut ai = 0;
    let mut bi = 0;
    while ai < a.len() && bi < b.len() {
        match a[ai].cmp(&b[bi]) {
            std::cmp::Ordering::Less => {
                result.push(a[ai]);
                ai += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[bi]);
                bi += 1;
            }
            std::cmp::Ordering::Equal => {
                result.push(a[ai]);
                ai += 1;
                bi += 1;
            }
        }
    }
    result.extend_from_slice(&a[ai..]);
    result.extend_from_slice(&b[bi..]);
    result
}

/// Sorted difference: elements in `a` not in `b`.
fn sorted_difference(a: &[ContentId], b: &[ContentId]) -> Vec<ContentId> {
    let mut result = Vec::with_capacity(a.len());
    let mut ai = 0;
    let mut bi = 0;
    while ai < a.len() && bi < b.len() {
        match a[ai].cmp(&b[bi]) {
            std::cmp::Ordering::Less => {
                result.push(a[ai]);
                ai += 1;
            }
            std::cmp::Ordering::Greater => {
                bi += 1;
            }
            std::cmp::Ordering::Equal => {
                ai += 1;
                bi += 1;
            }
        }
    }
    result.extend_from_slice(&a[ai..]);
    result
}

set_api!(
    RetainedSetStore,
    RetainedSetCursor,
    RetainedCollection,
    RetainedSetId,
    ObjectId,
    RetainedSetNode
);

impl<S: ImmutableObjectStore> RetainedSetStore<'_, S> {
    /// Compare two retained-object sets without writing derived collection nodes.
    ///
    /// Equal immutable subtrees are pruned by identity; divergent nodes are read
    /// and structurally validated at their contextual radix positions.
    pub async fn delta(
        &self,
        old: RetainedSetId,
        new: RetainedSetId,
        limits: RetainedSetDeltaLimits,
    ) -> Result<Vec<RetainedSetDelta>, CollectionError> {
        Ok(self
            .inner
            .paired_diff(
                old,
                new,
                radix::DiffLimits {
                    max_changes: limits.max_changes,
                    max_nodes: limits.max_nodes,
                    max_pending: limits.max_pending,
                },
            )
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(id, ()) => Some(RetainedSetDelta::Added(id)),
                radix::DiffEntry::Removed(id, ()) => Some(RetainedSetDelta::Removed(id)),
                radix::DiffEntry::Changed(_, (), ()) => None,
            })
            .collect())
    }
}
