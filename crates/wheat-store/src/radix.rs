//! Shared implementation for maximally path-compressed, fixed-byte radix collections.
//! The traits in this module are crate-private so collection identities cannot be mixed.

use std::{collections::BTreeMap, future::Future, marker::PhantomData, pin::Pin};

use wheat_core::{CanonicalObjectError, ObjectId};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError};

#[derive(Debug)]
pub(crate) enum Error {
    Store(ObjectStoreError),
    Decode(CanonicalObjectError),
    InvalidRootDepth,
    InvalidChildDepth,
    InvalidTerminalNode,
}

#[derive(Debug)]
pub(crate) enum DiffError {
    Radix(Error),
    ChangeLimitExceeded { limit: usize },
    NodeLimitExceeded { limit: usize },
    PendingLimitExceeded { limit: usize },
}

impl From<Error> for DiffError {
    fn from(value: Error) -> Self {
        Self::Radix(value)
    }
}

impl DiffError {
    /// The storage error behind a diff run with [`DiffLimits::unbounded`], which
    /// cannot exceed a limit.
    pub(crate) fn into_unbounded(self) -> Error {
        match self {
            Self::Radix(error) => error,
            Self::ChangeLimitExceeded { .. }
            | Self::NodeLimitExceeded { .. }
            | Self::PendingLimitExceeded { .. } => {
                unreachable!("an unbounded diff exceeded a limit")
            }
        }
    }
}

impl From<ObjectStoreError> for Error {
    fn from(value: ObjectStoreError) -> Self {
        Self::Store(value)
    }
}
impl From<CanonicalObjectError> for Error {
    fn from(value: CanonicalObjectError) -> Self {
        Self::Decode(value)
    }
}

pub(crate) trait Key: Copy + Ord + Send + Sync {
    const LEN: usize;
    fn bytes(&self) -> &[u8];
    fn from_bytes(bytes: &[u8]) -> Self;
}

#[derive(Clone)]
pub(crate) struct Node<I, V> {
    pub prefix: Vec<u8>,
    pub value: Option<V>,
    pub children: Vec<(u8, I)>,
    /// Inline entries: key suffixes with values stored directly in this node
    /// instead of as separate child objects.
    pub entries: Vec<(Vec<u8>, V)>,
}

/// Checks a node's shape at key byte `position`, for keys of `key_len` bytes: the
/// prefix stays within the key, children and inline entries end exactly at full
/// depth, a value only sits on a terminal node at full depth, and an interior node
/// branches at least twice (only an empty root may be bare).
pub(crate) fn validate_node_at<I, V>(
    node: &Node<I, V>,
    position: usize,
    key_len: usize,
) -> Result<(), Error> {
    let end = position
        .checked_add(node.prefix.len())
        .ok_or(Error::InvalidChildDepth)?;
    if end > key_len {
        return Err(Error::InvalidChildDepth);
    }
    // A child consumes one edge byte; an inline entry holds the rest of its key.
    if (end == key_len && !(node.children.is_empty() && node.entries.is_empty()))
        || node
            .entries
            .iter()
            .any(|(suffix, _)| suffix.len() != key_len - end)
    {
        return Err(Error::InvalidChildDepth);
    }
    let total_branches = node.children.len() + node.entries.len();
    if node.value.is_some() {
        // Terminal: at full key depth, no children or entries.
        if end != key_len || total_branches != 0 {
            return Err(Error::InvalidTerminalNode);
        }
    } else if total_branches == 1
        || (total_branches == 0 && !(position == 0 && node.prefix.is_empty()))
    {
        return Err(Error::InvalidTerminalNode);
    }
    Ok(())
}

pub(crate) trait Collection: Send + Sync {
    type Id: Copy + Eq + Into<ObjectId> + Send + Sync;
    type Key: Key;
    type Value: Copy + Eq + Send + Sync;
    type PublicNode: Clone + Send;

    const KIND: ObjectKind;
    fn decode(bytes: &[u8]) -> Result<Self::PublicNode, CanonicalObjectError>;
    fn unpack(node: Self::PublicNode) -> Node<Self::Id, Self::Value>;
    fn build(node: Node<Self::Id, Self::Value>) -> Result<Self::PublicNode, CanonicalObjectError>;
    fn id(node: &Self::PublicNode) -> Self::Id;
    fn canonical_bytes(node: &Self::PublicNode) -> Vec<u8>;
}

pub(crate) struct Store<'a, S, C> {
    objects: &'a S,
    marker: PhantomData<C>,
}
impl<'a, S, C> Copy for Store<'a, S, C> {}
impl<'a, S, C> Clone for Store<'a, S, C> {
    fn clone(&self) -> Self {
        *self
    }
}

type BuildFuture<'a, I> = Pin<Box<dyn Future<Output = Result<I, Error>> + Send + 'a>>;
type UpdateFuture<'a, I> = Pin<Box<dyn Future<Output = Result<Option<I>, Error>> + Send + 'a>>;
type Frame<C> = (
    Node<<C as Collection>::Id, <C as Collection>::Value>,
    usize,
    usize,
    usize,
    bool,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiffLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_changes: 250_000,
            max_nodes: 500_000,
            // A radix node has at most 256 children; leave room for several
            // divergent levels while still bounding adversarial inputs.
            max_pending: 4_096,
        }
    }
}

impl DiffLimits {
    /// No limits, for callers whose inputs are already authenticated local roots.
    pub(crate) const fn unbounded() -> Self {
        Self {
            max_changes: usize::MAX,
            max_nodes: usize::MAX,
            max_pending: usize::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffEntry<K, V> {
    Added(K, V),
    Changed(K, V, V),
    Removed(K, V),
}

enum DiffSide<C: Collection> {
    Id(C::Id),
    Loaded(Node<C::Id, C::Value>, usize),
}

enum DiffTask<C: Collection> {
    Pair(DiffSide<C>, DiffSide<C>, usize, Option<u8>),
    One(DiffSide<C>, usize, Option<u8>, bool),
}

pub(crate) struct Cursor<'a, S, C: Collection> {
    store: Store<'a, S, C>,
    stack: Vec<Frame<C>>,
    key: Vec<u8>,
}

/// A post-order, context-aware radix traversal used by repository verification.
///
/// Every node is read through [`Store::read_at`], so its structural constraints are
/// checked at its actual key position. Callers can prune a subtree only after a
/// receipt for this exact reconstructed prefix and external validation context.
pub(crate) struct ContextWalk<'a, S, C: Collection> {
    store: Store<'a, S, C>,
    stack: Vec<ContextFrame<C>>,
    key: Vec<u8>,
    prefix_len: usize,
}

struct ContextFrame<C: Collection> {
    id: C::Id,
    node: Node<C::Id, C::Value>,
    position: usize,
    child_index: usize,
    entry_index: usize,
    yielded_value: bool,
    announced: bool,
}

pub(crate) enum ContextWalkStep<C: Collection> {
    Node { id: C::Id },
    Entry { key: C::Key, value: C::Value },
    Complete { id: C::Id },
}

/// One post-order step for two collections with the same fixed-width key.
///
/// Nodes are authenticated at their physical key positions before being exposed.
/// A caller may prune only the pair announced by the preceding `Node` step.
pub(crate) enum PairedContextWalkStep<L: Collection, R: Collection<Key = L::Key>> {
    Node {
        left: L::Id,
        right: Option<R::Id>,
        right_skip: usize,
    },
    Entry {
        left: L::Value,
        right: Option<R::Value>,
    },
    Complete {
        left: L::Id,
        right: Option<R::Id>,
        right_skip: usize,
        conflict_free: bool,
    },
}

pub(crate) struct PairedContextWalk<'a, S, L: Collection, R: Collection<Key = L::Key>> {
    left: Store<'a, S, L>,
    right: Store<'a, S, R>,
    stack: Vec<PairedContextFrame<L, R>>,
    key: Vec<u8>,
    prefix_len: usize,
    max_changes: usize,
    max_nodes: usize,
    max_pending: usize,
    nodes_read: usize,
    entries_emitted: usize,
}

enum PairedRight<R: Collection> {
    Present {
        id: R::Id,
        node: Node<R::Id, R::Value>,
        skip: usize,
    },
    Absent,
}

struct PairedContextFrame<L: Collection, R: Collection<Key = L::Key>> {
    left_id: L::Id,
    left: Node<L::Id, L::Value>,
    right: PairedRight<R>,
    position: usize,
    child_index: usize,
    entry_index: usize,
    yielded_value: bool,
    announced: bool,
    prunable: bool,
    aligned: bool,
    conflict_free: bool,
}

impl<'a, S, C> Store<'a, S, C>
where
    S: ImmutableObjectStore,
    C: Collection,
{
    pub(crate) fn new(objects: &'a S) -> Self {
        Self {
            objects,
            marker: PhantomData,
        }
    }

    pub(crate) async fn empty_root(&self) -> Result<C::Id, Error> {
        self.write(Node {
            prefix: Vec::new(),
            value: None,
            children: Vec::new(),
            entries: Vec::new(),
        })
        .await
    }

    pub(crate) async fn write_public(&self, id: C::Id, bytes: Vec<u8>) -> Result<(), Error> {
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(())
    }

    pub(crate) async fn read_public(&self, id: C::Id) -> Result<C::PublicNode, Error> {
        let object = self.objects.get(id.into()).await?;
        if object.kind != C::KIND {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: C::KIND,
                actual: object.kind,
            }
            .into());
        }
        Ok(C::decode(&object.canonical_bytes)?)
    }

    pub(crate) async fn read_public_at(
        &self,
        id: C::Id,
        position: usize,
    ) -> Result<C::PublicNode, Error> {
        self.read_public_at_with_size(id, position)
            .await
            .map(|(node, _)| node)
    }

    pub(crate) async fn read_public_at_with_size(
        &self,
        id: C::Id,
        position: usize,
    ) -> Result<(C::PublicNode, usize), Error> {
        let object = self.objects.get(id.into()).await?;
        if object.kind != C::KIND {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: C::KIND,
                actual: object.kind,
            }
            .into());
        }
        let decoded_bytes = object.canonical_bytes.len();
        let public = C::decode(&object.canonical_bytes)?;
        let node = C::unpack(public.clone());
        self.validate_at(&node, position)?;
        Ok((public, decoded_bytes))
    }

    async fn read_at(&self, id: C::Id, position: usize) -> Result<Node<C::Id, C::Value>, Error> {
        let node = C::unpack(self.read_public(id).await?);
        self.validate_at(&node, position)?;
        Ok(node)
    }

    fn validate_at(&self, node: &Node<C::Id, C::Value>, position: usize) -> Result<(), Error> {
        validate_node_at(node, position, C::Key::LEN)
    }

    async fn root(&self, id: C::Id) -> Result<Node<C::Id, C::Value>, Error> {
        let node = self.read_at(id, 0).await?;
        if node.prefix.is_empty()
            && node.value.is_none()
            && node.children.len() + node.entries.len() == 1
        {
            return Err(Error::InvalidRootDepth);
        }
        Ok(node)
    }

    async fn write(&self, node: Node<C::Id, C::Value>) -> Result<C::Id, Error> {
        let public = C::build(node)?;
        let id = C::id(&public);
        self.objects
            .put_if_absent(id.into(), &C::canonical_bytes(&public))
            .await?;
        Ok(id)
    }

    pub(crate) async fn cursor(&self, root: C::Id) -> Result<Cursor<'a, S, C>, Error> {
        let node = self.root(root).await?;
        Ok(Cursor {
            store: Self::new(self.objects),
            stack: vec![(node, 0, 0, 0, false)],
            key: vec![0; C::Key::LEN],
        })
    }

    /// A cursor over the keys from `lower` on, in ascending order. Only the nodes on the
    /// path to `lower` and those holding later keys are read.
    pub(crate) async fn cursor_from(
        &self,
        root: C::Id,
        lower: &C::Key,
    ) -> Result<Cursor<'a, S, C>, Error> {
        let bound = lower.bytes();
        let mut stack = Vec::new();
        let mut node = self.root(root).await?;
        let mut position = 0;
        loop {
            let end = position + node.prefix.len();
            match node.prefix.as_slice().cmp(&bound[position..end]) {
                // Every key below this node comes before the bound.
                std::cmp::Ordering::Less => break,
                // Every key below this node comes after it.
                std::cmp::Ordering::Greater => {
                    stack.push((node, position, 0, 0, false));
                    break;
                }
                std::cmp::Ordering::Equal => {}
            }
            if end == C::Key::LEN {
                // The node holds the bound itself.
                stack.push((node, position, 0, 0, false));
                break;
            }
            let edge = bound[end];
            let child_index = node.children.partition_point(|(child, _)| *child < edge);
            let entry_index = node
                .entries
                .partition_point(|(suffix, _)| suffix.as_slice() < &bound[end..]);
            let descend = node
                .children
                .get(child_index)
                .filter(|(child, _)| *child == edge)
                .map(|(_, id)| *id);
            match descend {
                Some(id) => {
                    stack.push((node, position, child_index + 1, entry_index, true));
                    position = end + 1;
                    node = self.read_at(id, position).await?;
                }
                None => {
                    stack.push((node, position, child_index, entry_index, true));
                    break;
                }
            }
        }
        Ok(Cursor {
            store: Self::new(self.objects),
            stack,
            // Every byte on the path walked so far equals the bound's.
            key: bound.to_vec(),
        })
    }

    pub(crate) async fn context_walk(&self, root: C::Id) -> Result<ContextWalk<'a, S, C>, Error> {
        let node = self.root(root).await?;
        Ok(ContextWalk {
            store: Self::new(self.objects),
            stack: vec![ContextFrame {
                id: root,
                node,
                position: 0,
                child_index: 0,
                entry_index: 0,
                yielded_value: false,
                announced: false,
            }],
            key: vec![0; C::Key::LEN],
            prefix_len: 0,
        })
    }

    pub(crate) async fn paired_context_walk<R>(
        &self,
        left_root: C::Id,
        right: Store<'a, S, R>,
        right_root: R::Id,
        limits: DiffLimits,
    ) -> Result<PairedContextWalk<'a, S, C, R>, DiffError>
    where
        R: Collection<Key = C::Key>,
    {
        if limits.max_nodes < 2 {
            return Err(DiffError::NodeLimitExceeded {
                limit: limits.max_nodes,
            });
        }
        let left = self.root(left_root).await.map_err(DiffError::from)?;
        let right_node = right.root(right_root).await.map_err(DiffError::from)?;
        Ok(PairedContextWalk {
            left: Self::new(self.objects),
            right,
            stack: vec![PairedContextFrame {
                left_id: left_root,
                left,
                right: PairedRight::Present {
                    id: right_root,
                    node: right_node,
                    skip: 0,
                },
                position: 0,
                child_index: 0,
                entry_index: 0,
                yielded_value: false,
                announced: false,
                prunable: false,
                aligned: false,
                conflict_free: true,
            }],
            key: vec![0; C::Key::LEN],
            prefix_len: 0,
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
            nodes_read: 2,
            entries_emitted: 0,
        })
    }

    pub(crate) async fn get(&self, root: C::Id, key: C::Key) -> Result<Option<C::Value>, Error> {
        let mut position = 0;
        let mut node = self.root(root).await?;
        loop {
            let end = position + node.prefix.len();
            if key.bytes()[position..end] != node.prefix {
                return Ok(None);
            }
            if end == C::Key::LEN {
                return Ok(node.value);
            }
            let suffix = &key.bytes()[end..];
            // Check inline entries first — their suffix covers the remaining key bytes.
            if let Ok(index) = node
                .entries
                .binary_search_by(|(s, _)| s.as_slice().cmp(suffix))
            {
                return Ok(Some(node.entries[index].1));
            }
            let edge = key.bytes()[end];
            let Ok(index) = node.children.binary_search_by_key(&edge, |child| child.0) else {
                return Ok(None);
            };
            position = end + 1;
            node = self.read_at(node.children[index].1, position).await?;
        }
    }

    #[tracing::instrument(
        name = "radix_update",
        level = "debug",
        skip_all,
        fields(collection = std::any::type_name::<C>().rsplit("::").next())
    )]
    pub(crate) async fn update_batch(
        &self,
        root: C::Id,
        edits: impl IntoIterator<Item = (C::Key, Option<C::Value>)>,
    ) -> Result<C::Id, Error> {
        let edits: Vec<_> = edits
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        if edits.is_empty() {
            // Authenticate the supplied root even for an empty update.
            self.root(root).await?;
            return Ok(root);
        }
        let initial = self.root(root).await?;
        let initially_empty = initial.prefix.is_empty()
            && initial.value.is_none()
            && initial.children.is_empty()
            && initial.entries.is_empty();
        if initially_empty {
            let present = edits
                .iter()
                .filter_map(|(key, value)| value.map(|value| (*key, value)))
                .collect::<Vec<_>>();
            return if present.is_empty() {
                Ok(root)
            } else {
                self.build_subtree(0, &present).await
            };
        }
        let mut current = root;
        for edit in &edits {
            if self.get(current, edit.0).await? == edit.1 {
                continue;
            }
            let node = self.root(current).await?;
            let was_empty = node.prefix.is_empty()
                && node.value.is_none()
                && node.children.is_empty()
                && node.entries.is_empty();
            if was_empty {
                let Some(value) = edit.1 else { continue };
                current = self
                    .write(Node {
                        prefix: edit.0.bytes().to_vec(),
                        value: Some(value),
                        children: Vec::new(),
                        entries: Vec::new(),
                    })
                    .await?;
                continue;
            }
            current = self
                .update_node(Some(current), node, 0, std::slice::from_ref(edit))
                .await?
                .expect("root is never pruned");
        }
        Ok(current)
    }

    /// Recursively update a node with a batch of edits, producing canonical
    /// inline-entry structure.  Returns `None` when the node is pruned (all
    /// entries removed from a non-root subtree).
    fn update_node<'b>(
        &'b self,
        original: Option<C::Id>,
        node: Node<C::Id, C::Value>,
        position: usize,
        edits: &'b [(C::Key, Option<C::Value>)],
    ) -> UpdateFuture<'b, C::Id> {
        Box::pin(async move {
            if edits.is_empty() {
                return Ok(original);
            }
            let common = edits
                .iter()
                .map(|(key, _)| {
                    node.prefix
                        .iter()
                        .zip(&key.bytes()[position..])
                        .take_while(|(left, right)| left == right)
                        .count()
                })
                .min()
                .expect("edits are non-empty");

            // ── Prefix diverges: split this node ──────────────────────────
            if common < node.prefix.len() {
                if edits.iter().all(|(_, value)| value.is_none()) {
                    return Ok(original);
                }
                let split = position + common;
                let old_edge = node.prefix[common];

                // Determine whether the old node (after prefix split) is a
                // bare terminal leaf that should be inlined.
                let old_remainder = &node.prefix[common + 1..];
                let old_is_terminal_leaf = node.value.is_some()
                    && node.children.is_empty()
                    && node.entries.is_empty()
                    && position + node.prefix.len() == C::Key::LEN;

                let mut new_children: Vec<(u8, C::Id)> = Vec::new();
                let mut new_entries: Vec<(Vec<u8>, C::Value)> = Vec::new();

                if old_is_terminal_leaf {
                    // The old node becomes an inline entry (edge + remainder).
                    let mut suffix = Vec::with_capacity(1 + old_remainder.len());
                    suffix.push(old_edge);
                    suffix.extend_from_slice(old_remainder);
                    new_entries.push((suffix, node.value.unwrap()));
                } else {
                    let old_child = self
                        .write(Node {
                            prefix: old_remainder.to_vec(),
                            value: node.value,
                            children: node.children,
                            entries: node.entries,
                        })
                        .await?;
                    new_children.push((old_edge, old_child));
                }

                // Process edit groups by edge byte at the split position.
                let mut start = 0;
                while start < edits.len() {
                    let edge = edits[start].0.bytes()[split];
                    let group_end = start
                        + edits[start..].partition_point(|(key, _)| key.bytes()[split] == edge);
                    let present: Vec<_> = edits[start..group_end]
                        .iter()
                        .filter_map(|(key, value)| value.map(|value| (*key, value)))
                        .collect();
                    if !present.is_empty() {
                        if present.len() == 1 {
                            // Single entry: inline it.
                            let suffix = present[0].0.bytes()[split..].to_vec();
                            new_entries.push((suffix, present[0].1));
                        } else {
                            let child = self.build_subtree(split + 1, &present).await?;
                            new_children.push((edge, child));
                        }
                    }
                    start = group_end;
                }

                let total = new_children.len() + new_entries.len();
                if total <= 1 && position > 0 {
                    // After adding edits, if we still have only the old node
                    // (all edits were deletions of non-existent keys), nothing
                    // changed.
                    if total == 1 && new_entries.is_empty() && new_children[0].0 == old_edge {
                        return Ok(original);
                    }
                }
                new_children.sort_by_key(|&(e, _)| e);
                new_entries.sort_by(|a, b| a.0.cmp(&b.0));
                return Ok(Some(
                    self.write(Node {
                        prefix: node.prefix[..common].to_vec(),
                        value: None,
                        children: new_children,
                        entries: new_entries,
                    })
                    .await?,
                ));
            }

            // ── Full prefix match, terminal depth ─────────────────────────
            let end = position + node.prefix.len();
            if end == C::Key::LEN {
                let value = edits[edits.len() - 1].1;
                if value == node.value {
                    return Ok(original);
                }
                return match value {
                    Some(value) => Ok(Some(
                        self.write(Node {
                            prefix: node.prefix,
                            value: Some(value),
                            children: Vec::new(),
                            entries: Vec::new(),
                        })
                        .await?,
                    )),
                    None if position == 0 => Ok(Some(self.empty_root().await?)),
                    None => Ok(None),
                };
            }

            // ── Full prefix match, non-terminal: update children & entries ─
            let mut children = node.children;
            let mut entries = node.entries;
            let mut changed = false;
            let mut start = 0;
            while start < edits.len() {
                let edge = edits[start].0.bytes()[end];
                let group_end =
                    start + edits[start..].partition_point(|(key, _)| key.bytes()[end] == edge);
                let child_location = children.binary_search_by_key(&edge, |child| child.0);

                // Find an existing inline entry whose first suffix byte matches
                // the edit's edge byte (not exact suffix match — entries under the
                // same discriminating byte must interact).
                let entry_idx = entries
                    .iter()
                    .position(|(s, _)| s.first().copied() == Some(edge));

                if let Some(entry_idx) = entry_idx {
                    // Edit group shares first byte with an existing inline entry.
                    let edit_suffix = &edits[start].0.bytes()[end..];
                    if group_end - start == 1 && entries[entry_idx].0 == edit_suffix {
                        // Single edit targeting this exact entry — update in place.
                        match edits[start].1 {
                            Some(value) => {
                                if entries[entry_idx].1 != value {
                                    entries[entry_idx].1 = value;
                                    changed = true;
                                }
                            }
                            None => {
                                entries.remove(entry_idx);
                                changed = true;
                            }
                        }
                    } else {
                        // Edits interact with this entry's subtree. Pull the entry
                        // out and merge with the edits to rebuild.
                        let (old_suffix, old_value) = entries.remove(entry_idx);
                        changed = true;
                        // Reconstruct the full key for the old inline entry.
                        let mut full_key = vec![0u8; C::Key::LEN];
                        full_key[..end].copy_from_slice(&edits[start].0.bytes()[..end]);
                        full_key[end..].copy_from_slice(&old_suffix);
                        let mut combined = vec![(C::Key::from_bytes(&full_key), Some(old_value))];
                        combined.extend_from_slice(&edits[start..group_end]);
                        // Dedup/apply: last value for each key wins.
                        let present: Vec<_> = combined
                            .iter()
                            .filter_map(|(key, value)| value.map(|v| (*key, v)))
                            .collect::<BTreeMap<_, _>>()
                            .into_iter()
                            .collect();
                        if present.len() == 1 {
                            // Still a single entry: re-inline.
                            let suffix = present[0].0.bytes()[end..].to_vec();
                            let ins_pos = entries
                                .binary_search_by(|(s, _)| s.as_slice().cmp(&suffix))
                                .unwrap_err();
                            entries.insert(ins_pos, (suffix, present[0].1));
                        } else if !present.is_empty() {
                            // Multiple entries: create a child subtree.
                            let child = self.build_subtree(end + 1, &present).await?;
                            let ins_pos =
                                children.binary_search_by_key(&edge, |c| c.0).unwrap_err();
                            children.insert(ins_pos, (edge, child));
                        }
                    }
                } else if let Ok(child_idx) = child_location {
                    // Edit targets an existing child — recurse.
                    let old_id = children[child_idx].1;
                    let child_node = self.read_at(old_id, end + 1).await?;
                    let replacement = self
                        .update_node(Some(old_id), child_node, end + 1, &edits[start..group_end])
                        .await?;
                    if replacement != Some(old_id) {
                        changed = true;
                        match replacement {
                            Some(new_id) => {
                                // Check if the child is now a single bare leaf
                                // that should be promoted to an inline entry.
                                let updated = self.read_at(new_id, end + 1).await?;
                                if let Some(leaf_value) = updated.value {
                                    if updated.children.is_empty()
                                        && updated.entries.is_empty()
                                        && end + 1 + updated.prefix.len() == C::Key::LEN
                                    {
                                        // Promote to inline entry.
                                        children.remove(child_idx);
                                        let mut suffix =
                                            Vec::with_capacity(1 + updated.prefix.len());
                                        suffix.push(edge);
                                        suffix.extend_from_slice(&updated.prefix);
                                        let ins_pos = entries
                                            .binary_search_by(|(s, _)| s.as_slice().cmp(&suffix))
                                            .unwrap_err();
                                        entries.insert(ins_pos, (suffix, leaf_value));
                                    } else {
                                        children[child_idx].1 = new_id;
                                    }
                                } else {
                                    children[child_idx].1 = new_id;
                                }
                            }
                            None => {
                                children.remove(child_idx);
                            }
                        }
                    }
                } else {
                    // No existing child or entry at this edge byte — create new.
                    let present: Vec<_> = edits[start..group_end]
                        .iter()
                        .filter_map(|(key, value)| value.map(|value| (*key, value)))
                        .collect();
                    if !present.is_empty() {
                        changed = true;
                        if present.len() == 1 {
                            // Single entry: inline it.
                            let suffix = present[0].0.bytes()[end..].to_vec();
                            let ins_pos = entries
                                .binary_search_by(|(s, _)| s.as_slice().cmp(&suffix))
                                .unwrap_err();
                            entries.insert(ins_pos, (suffix, present[0].1));
                        } else {
                            let child = self.build_subtree(end + 1, &present).await?;
                            let ins_pos =
                                children.binary_search_by_key(&edge, |c| c.0).unwrap_err();
                            children.insert(ins_pos, (edge, child));
                        }
                    }
                }
                start = group_end;
            }
            if !changed {
                return Ok(original);
            }
            let total_branches = children.len() + entries.len();
            if total_branches == 0 {
                return if position == 0 {
                    Ok(Some(self.empty_root().await?))
                } else {
                    Ok(None)
                };
            }
            if total_branches == 1 && node.value.is_none() {
                // Collapse: merge the single remaining branch into this node.
                if let Some((edge, child_id)) = children.first().copied() {
                    let child = self.read_at(child_id, end + 1).await?;
                    let mut prefix = node.prefix;
                    prefix.reserve(1 + child.prefix.len());
                    prefix.push(edge);
                    prefix.extend_from_slice(&child.prefix);
                    return Ok(Some(
                        self.write(Node {
                            prefix,
                            value: child.value,
                            children: child.children,
                            entries: child.entries,
                        })
                        .await?,
                    ));
                } else {
                    // Single remaining entry: collapse into a terminal node.
                    let (suffix, value) = entries.into_iter().next().unwrap();
                    let mut prefix = node.prefix;
                    prefix.extend_from_slice(&suffix);
                    return Ok(Some(
                        self.write(Node {
                            prefix,
                            value: Some(value),
                            children: Vec::new(),
                            entries: Vec::new(),
                        })
                        .await?,
                    ));
                }
            }
            Ok(Some(
                self.write(Node {
                    prefix: node.prefix,
                    value: node.value,
                    children,
                    entries,
                })
                .await?,
            ))
        })
    }

    /// Compares two persistent roots without reading subtrees whose identities match.
    ///
    /// Traversal is iterative, lexicographically ordered, and bounded independently
    /// by decoded nodes, pending work, and reported changes.
    pub(crate) async fn paired_diff(
        &self,
        left: C::Id,
        right: C::Id,
        limits: DiffLimits,
    ) -> Result<Vec<DiffEntry<C::Key, C::Value>>, DiffError> {
        if left == right {
            return Ok(Vec::new());
        }

        let mut pending = Vec::with_capacity(C::Key::LEN * 2);
        pending.push(DiffTask::Pair(
            DiffSide::Id(left),
            DiffSide::Id(right),
            0,
            None,
        ));
        let mut key = vec![0; C::Key::LEN];
        let mut changes = Vec::new();
        let mut nodes_read = 0usize;

        while let Some(task) = pending.pop() {
            match task {
                DiffTask::One(side, position, edge, added) => {
                    if let Some(edge) = edge {
                        key[position - 1] = edge;
                    }
                    let (node, skip) = self
                        .diff_load(side, position, &mut nodes_read, limits)
                        .await?;
                    let end = position + node.prefix.len() - skip;
                    key[position..end].copy_from_slice(&node.prefix[skip..]);
                    if let Some(value) = node.value {
                        Self::diff_emit(
                            &mut changes,
                            if added {
                                DiffEntry::Added(C::Key::from_bytes(&key), value)
                            } else {
                                DiffEntry::Removed(C::Key::from_bytes(&key), value)
                            },
                            limits,
                        )?;
                    }
                    // Emit inline entries.
                    for (suffix, value) in &node.entries {
                        key[end..end + suffix.len()].copy_from_slice(suffix);
                        Self::diff_emit(
                            &mut changes,
                            if added {
                                DiffEntry::Added(C::Key::from_bytes(&key), *value)
                            } else {
                                DiffEntry::Removed(C::Key::from_bytes(&key), *value)
                            },
                            limits,
                        )?;
                    }
                    for &(edge, id) in node.children.iter().rev() {
                        pending.push(DiffTask::One(DiffSide::Id(id), end + 1, Some(edge), added));
                    }
                }
                DiffTask::Pair(left_side, right_side, position, edge) => {
                    if let Some(edge) = edge {
                        key[position - 1] = edge;
                    }
                    if let (DiffSide::Id(left), DiffSide::Id(right)) = (&left_side, &right_side)
                        && left == right
                    {
                        continue;
                    }
                    let (left, left_skip) = self
                        .diff_load(left_side, position, &mut nodes_read, limits)
                        .await?;
                    let (right, right_skip) = self
                        .diff_load(right_side, position, &mut nodes_read, limits)
                        .await?;
                    let left_prefix = &left.prefix[left_skip..];
                    let right_prefix = &right.prefix[right_skip..];
                    let common = left_prefix
                        .iter()
                        .zip(right_prefix)
                        .take_while(|(left, right)| left == right)
                        .count();
                    key[position..position + common].copy_from_slice(&left_prefix[..common]);

                    if common < left_prefix.len() && common < right_prefix.len() {
                        let (first, second) = if left_prefix[common] < right_prefix[common] {
                            (
                                DiffTask::One(
                                    DiffSide::Loaded(left, left_skip + common),
                                    position + common,
                                    None,
                                    false,
                                ),
                                DiffTask::One(
                                    DiffSide::Loaded(right, right_skip + common),
                                    position + common,
                                    None,
                                    true,
                                ),
                            )
                        } else {
                            (
                                DiffTask::One(
                                    DiffSide::Loaded(right, right_skip + common),
                                    position + common,
                                    None,
                                    true,
                                ),
                                DiffTask::One(
                                    DiffSide::Loaded(left, left_skip + common),
                                    position + common,
                                    None,
                                    false,
                                ),
                            )
                        };
                        pending.push(second);
                        pending.push(first);
                    } else if common == left_prefix.len() && common == right_prefix.len() {
                        let end = position + common;
                        match (left.value, right.value) {
                            (Some(old), Some(new)) if old != new => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Changed(C::Key::from_bytes(&key), old, new),
                                limits,
                            )?,
                            (Some(old), None) => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Removed(C::Key::from_bytes(&key), old),
                                limits,
                            )?,
                            (None, Some(new)) => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Added(C::Key::from_bytes(&key), new),
                                limits,
                            )?,
                            _ => {}
                        }
                        // Merge-diff all branches (children + entries) from both sides
                        // by discriminating byte. A byte can be a child on one side
                        // and an inline entry on the other (structural difference),
                        // so we must cross-match.
                        #[derive(Clone, Copy)]
                        enum Branch<I, V: Copy> {
                            Child(u8, I),
                            Entry(u8, V), // first suffix byte + value
                        }
                        impl<I: Copy, V: Copy> Branch<I, V> {
                            fn byte(&self) -> u8 {
                                match self {
                                    Branch::Child(b, _) | Branch::Entry(b, _) => *b,
                                }
                            }
                        }
                        // Collect and sort by discriminating byte (they're already
                        // sorted within each list; merge is O(n)).
                        let left_branches: Vec<Branch<C::Id, C::Value>> = {
                            let mut v =
                                Vec::with_capacity(left.children.len() + left.entries.len());
                            let mut ci = 0;
                            let mut ei = 0;
                            loop {
                                let cb = left.children.get(ci).map(|c| c.0);
                                let eb = left.entries.get(ei).and_then(|(s, _)| s.first().copied());
                                match (cb, eb) {
                                    (Some(c), Some(e)) if c < e => {
                                        v.push(Branch::Child(c, left.children[ci].1));
                                        ci += 1;
                                    }
                                    (Some(_), Some(e)) => {
                                        v.push(Branch::Entry(e, left.entries[ei].1));
                                        ei += 1;
                                    }
                                    (Some(c), None) => {
                                        v.push(Branch::Child(c, left.children[ci].1));
                                        ci += 1;
                                    }
                                    (None, Some(e)) => {
                                        v.push(Branch::Entry(e, left.entries[ei].1));
                                        ei += 1;
                                    }
                                    (None, None) => break,
                                }
                            }
                            v
                        };
                        let right_branches: Vec<Branch<C::Id, C::Value>> = {
                            let mut v =
                                Vec::with_capacity(right.children.len() + right.entries.len());
                            let mut ci = 0;
                            let mut ei = 0;
                            loop {
                                let cb = right.children.get(ci).map(|c| c.0);
                                let eb =
                                    right.entries.get(ei).and_then(|(s, _)| s.first().copied());
                                match (cb, eb) {
                                    (Some(c), Some(e)) if c < e => {
                                        v.push(Branch::Child(c, right.children[ci].1));
                                        ci += 1;
                                    }
                                    (Some(_), Some(e)) => {
                                        v.push(Branch::Entry(e, right.entries[ei].1));
                                        ei += 1;
                                    }
                                    (Some(c), None) => {
                                        v.push(Branch::Child(c, right.children[ci].1));
                                        ci += 1;
                                    }
                                    (None, Some(e)) => {
                                        v.push(Branch::Entry(e, right.entries[ei].1));
                                        ei += 1;
                                    }
                                    (None, None) => break,
                                }
                            }
                            v
                        };
                        // Now merge-diff the two branch lists by byte.
                        // Entry diffs are collected here (loop is reverse, but
                        // entries must be emitted in forward key order).
                        let mut deferred_entry_diffs: Vec<DiffEntry<C::Key, C::Value>> = Vec::new();
                        let mut lbi = left_branches.len();
                        let mut rbi = right_branches.len();
                        let left_entry_by_byte = |byte: u8| -> Option<usize> {
                            left.entries.binary_search_by(|(s, _)| s[0].cmp(&byte)).ok()
                        };
                        let right_entry_by_byte = |byte: u8| -> Option<usize> {
                            right
                                .entries
                                .binary_search_by(|(s, _)| s[0].cmp(&byte))
                                .ok()
                        };
                        while lbi != 0 || rbi != 0 {
                            let lb = lbi.checked_sub(1).map(|i| &left_branches[i]);
                            let rb = rbi.checked_sub(1).map(|i| &right_branches[i]);
                            match (lb, rb) {
                                (Some(l), Some(r)) if l.byte() == r.byte() => {
                                    let byte = l.byte();
                                    lbi -= 1;
                                    rbi -= 1;
                                    match (l, r) {
                                        (Branch::Child(_, li), Branch::Child(_, ri)) => {
                                            if li != ri {
                                                pending.push(DiffTask::Pair(
                                                    DiffSide::Id(*li),
                                                    DiffSide::Id(*ri),
                                                    end + 1,
                                                    Some(byte),
                                                ));
                                            }
                                        }
                                        (Branch::Entry(_, lv), Branch::Entry(_, rv)) => {
                                            // Both are entries — compare full suffixes.
                                            let lei = left_entry_by_byte(byte).unwrap();
                                            let rei = right_entry_by_byte(byte).unwrap();
                                            let (ls, _) = &left.entries[lei];
                                            let (rs, _) = &right.entries[rei];
                                            if ls == rs {
                                                if lv != rv {
                                                    key[end..end + ls.len()].copy_from_slice(ls);
                                                    deferred_entry_diffs.push(DiffEntry::Changed(
                                                        C::Key::from_bytes(&key),
                                                        *lv,
                                                        *rv,
                                                    ));
                                                }
                                            } else {
                                                // Same first byte, different suffixes.
                                                key[end..end + ls.len()].copy_from_slice(ls);
                                                deferred_entry_diffs.push(DiffEntry::Removed(
                                                    C::Key::from_bytes(&key),
                                                    *lv,
                                                ));
                                                key[end..end + rs.len()].copy_from_slice(rs);
                                                deferred_entry_diffs.push(DiffEntry::Added(
                                                    C::Key::from_bytes(&key),
                                                    *rv,
                                                ));
                                            }
                                        }
                                        (Branch::Child(_, id), Branch::Entry(_, rv)) => {
                                            // Left has a child, right has an entry.
                                            // Pair: make entry a synthetic node.
                                            let rei = right_entry_by_byte(byte).unwrap();
                                            let (rs, _) = &right.entries[rei];
                                            let entry_node = Node {
                                                prefix: rs[1..].to_vec(),
                                                value: Some(*rv),
                                                children: Vec::new(),
                                                entries: Vec::new(),
                                            };
                                            pending.push(DiffTask::Pair(
                                                DiffSide::Id(*id),
                                                DiffSide::Loaded(entry_node, 0),
                                                end + 1,
                                                Some(byte),
                                            ));
                                        }
                                        (Branch::Entry(_, lv), Branch::Child(_, id)) => {
                                            // Left has an entry, right has a child.
                                            let lei = left_entry_by_byte(byte).unwrap();
                                            let (ls, _) = &left.entries[lei];
                                            let entry_node = Node {
                                                prefix: ls[1..].to_vec(),
                                                value: Some(*lv),
                                                children: Vec::new(),
                                                entries: Vec::new(),
                                            };
                                            pending.push(DiffTask::Pair(
                                                DiffSide::Loaded(entry_node, 0),
                                                DiffSide::Id(*id),
                                                end + 1,
                                                Some(byte),
                                            ));
                                        }
                                    }
                                }
                                (Some(l), Some(r)) if l.byte() > r.byte() => {
                                    lbi -= 1;
                                    let byte = l.byte();
                                    match l {
                                        Branch::Child(_, id) => {
                                            pending.push(DiffTask::One(
                                                DiffSide::Id(*id),
                                                end + 1,
                                                Some(byte),
                                                false,
                                            ));
                                        }
                                        Branch::Entry(_, val) => {
                                            let lei = left_entry_by_byte(byte).unwrap();
                                            let (ls, _) = &left.entries[lei];
                                            key[end..end + ls.len()].copy_from_slice(ls);
                                            deferred_entry_diffs.push(DiffEntry::Removed(
                                                C::Key::from_bytes(&key),
                                                *val,
                                            ));
                                        }
                                    }
                                }
                                (Some(_), Some(r)) => {
                                    rbi -= 1;
                                    let byte = r.byte();
                                    match r {
                                        Branch::Child(_, id) => {
                                            pending.push(DiffTask::One(
                                                DiffSide::Id(*id),
                                                end + 1,
                                                Some(byte),
                                                true,
                                            ));
                                        }
                                        Branch::Entry(_, val) => {
                                            let rei = right_entry_by_byte(byte).unwrap();
                                            let (rs, _) = &right.entries[rei];
                                            key[end..end + rs.len()].copy_from_slice(rs);
                                            deferred_entry_diffs.push(DiffEntry::Added(
                                                C::Key::from_bytes(&key),
                                                *val,
                                            ));
                                        }
                                    }
                                }
                                (Some(l), None) => {
                                    lbi -= 1;
                                    let byte = l.byte();
                                    match l {
                                        Branch::Child(_, id) => {
                                            pending.push(DiffTask::One(
                                                DiffSide::Id(*id),
                                                end + 1,
                                                Some(byte),
                                                false,
                                            ));
                                        }
                                        Branch::Entry(_, val) => {
                                            let lei = left_entry_by_byte(byte).unwrap();
                                            let (ls, _) = &left.entries[lei];
                                            key[end..end + ls.len()].copy_from_slice(ls);
                                            deferred_entry_diffs.push(DiffEntry::Removed(
                                                C::Key::from_bytes(&key),
                                                *val,
                                            ));
                                        }
                                    }
                                }
                                (None, Some(r)) => {
                                    rbi -= 1;
                                    let byte = r.byte();
                                    match r {
                                        Branch::Child(_, id) => {
                                            pending.push(DiffTask::One(
                                                DiffSide::Id(*id),
                                                end + 1,
                                                Some(byte),
                                                true,
                                            ));
                                        }
                                        Branch::Entry(_, val) => {
                                            let rei = right_entry_by_byte(byte).unwrap();
                                            let (rs, _) = &right.entries[rei];
                                            key[end..end + rs.len()].copy_from_slice(rs);
                                            deferred_entry_diffs.push(DiffEntry::Added(
                                                C::Key::from_bytes(&key),
                                                *val,
                                            ));
                                        }
                                    }
                                }
                                (None, None) => break,
                            }
                        }
                        // Emit deferred entry diffs in forward (key-sorted) order.
                        // The loop above ran high→low, so reverse.
                        for entry_diff in deferred_entry_diffs.into_iter().rev() {
                            Self::diff_emit(&mut changes, entry_diff, limits)?;
                        }
                    } else if common == left_prefix.len() {
                        let end = position + common;
                        let edge = right_prefix[common];
                        let matching = left
                            .children
                            .binary_search_by_key(&edge, |child| child.0)
                            .ok();
                        // Check if any left entry has first byte matching the right's edge.
                        let matching_entry = left
                            .entries
                            .binary_search_by(|(suffix, _)| suffix[0].cmp(&edge))
                            .ok();
                        let mut divergent = Some(right);
                        for (index, &(child_edge, id)) in left.children.iter().enumerate().rev() {
                            if matching.is_none()
                                && matching_entry.is_none()
                                && child_edge < edge
                                && divergent.is_some()
                            {
                                pending.push(DiffTask::One(
                                    DiffSide::Loaded(
                                        divergent.take().expect("divergent side is queued once"),
                                        right_skip + common + 1,
                                    ),
                                    end + 1,
                                    Some(edge),
                                    true,
                                ));
                            }
                            pending.push(if Some(index) == matching {
                                DiffTask::Pair(
                                    DiffSide::Id(id),
                                    DiffSide::Loaded(
                                        divergent.take().expect("matching side is queued once"),
                                        right_skip + common + 1,
                                    ),
                                    end + 1,
                                    Some(edge),
                                )
                            } else {
                                DiffTask::One(DiffSide::Id(id), end + 1, Some(child_edge), false)
                            });
                        }
                        // Emit only-left inline entries, except the one that
                        // matches the right's edge — that gets paired with
                        // the right subtree as a synthetic terminal node.
                        for (i, (suffix, value)) in left.entries.iter().enumerate() {
                            if Some(i) == matching_entry {
                                // Convert the entry into a synthetic terminal node
                                // and pair it with the right subtree.
                                let entry_node = Node {
                                    prefix: suffix[1..].to_vec(),
                                    value: Some(*value),
                                    children: Vec::new(),
                                    entries: Vec::new(),
                                };
                                if let Some(right) = divergent.take() {
                                    pending.push(DiffTask::Pair(
                                        DiffSide::Loaded(entry_node, 0),
                                        DiffSide::Loaded(right, right_skip + common + 1),
                                        end + 1,
                                        Some(edge),
                                    ));
                                } else {
                                    // Right was already enqueued as a child match —
                                    // shouldn't happen since entries and children are
                                    // disjoint, but handle gracefully.
                                    key[end..end + suffix.len()].copy_from_slice(suffix);
                                    Self::diff_emit(
                                        &mut changes,
                                        DiffEntry::Removed(C::Key::from_bytes(&key), *value),
                                        limits,
                                    )?;
                                }
                            } else {
                                key[end..end + suffix.len()].copy_from_slice(suffix);
                                Self::diff_emit(
                                    &mut changes,
                                    DiffEntry::Removed(C::Key::from_bytes(&key), *value),
                                    limits,
                                )?;
                            }
                        }
                        if let Some(divergent) = divergent {
                            pending.push(DiffTask::One(
                                DiffSide::Loaded(divergent, right_skip + common + 1),
                                end + 1,
                                Some(edge),
                                true,
                            ));
                        }
                    } else {
                        let end = position + common;
                        let edge = left_prefix[common];
                        let matching = right
                            .children
                            .binary_search_by_key(&edge, |child| child.0)
                            .ok();
                        let matching_entry = right
                            .entries
                            .binary_search_by(|(suffix, _)| suffix[0].cmp(&edge))
                            .ok();
                        let mut divergent = Some(left);
                        for (index, &(child_edge, id)) in right.children.iter().enumerate().rev() {
                            if matching.is_none()
                                && matching_entry.is_none()
                                && child_edge < edge
                                && divergent.is_some()
                            {
                                pending.push(DiffTask::One(
                                    DiffSide::Loaded(
                                        divergent.take().expect("divergent side is queued once"),
                                        left_skip + common + 1,
                                    ),
                                    end + 1,
                                    Some(edge),
                                    false,
                                ));
                            }
                            pending.push(if Some(index) == matching {
                                DiffTask::Pair(
                                    DiffSide::Loaded(
                                        divergent.take().expect("matching side is queued once"),
                                        left_skip + common + 1,
                                    ),
                                    DiffSide::Id(id),
                                    end + 1,
                                    Some(edge),
                                )
                            } else {
                                DiffTask::One(DiffSide::Id(id), end + 1, Some(child_edge), true)
                            });
                        }
                        // Handle right inline entries — pair matching ones with the
                        // left subtree, emit non-matching as Added.
                        for (i, (suffix, value)) in right.entries.iter().enumerate() {
                            if Some(i) == matching_entry {
                                let entry_node = Node {
                                    prefix: suffix[1..].to_vec(),
                                    value: Some(*value),
                                    children: Vec::new(),
                                    entries: Vec::new(),
                                };
                                if let Some(left) = divergent.take() {
                                    pending.push(DiffTask::Pair(
                                        DiffSide::Loaded(left, left_skip + common + 1),
                                        DiffSide::Loaded(entry_node, 0),
                                        end + 1,
                                        Some(edge),
                                    ));
                                } else {
                                    key[end..end + suffix.len()].copy_from_slice(suffix);
                                    Self::diff_emit(
                                        &mut changes,
                                        DiffEntry::Added(C::Key::from_bytes(&key), *value),
                                        limits,
                                    )?;
                                }
                            } else {
                                key[end..end + suffix.len()].copy_from_slice(suffix);
                                Self::diff_emit(
                                    &mut changes,
                                    DiffEntry::Added(C::Key::from_bytes(&key), *value),
                                    limits,
                                )?;
                            }
                        }
                        if let Some(divergent) = divergent {
                            pending.push(DiffTask::One(
                                DiffSide::Loaded(divergent, left_skip + common + 1),
                                end + 1,
                                Some(edge),
                                false,
                            ));
                        }
                    }
                }
            }
            if pending.len() > limits.max_pending {
                return Err(DiffError::PendingLimitExceeded {
                    limit: limits.max_pending,
                });
            }
        }
        Ok(changes)
    }

    async fn diff_load(
        &self,
        side: DiffSide<C>,
        position: usize,
        nodes_read: &mut usize,
        limits: DiffLimits,
    ) -> Result<(Node<C::Id, C::Value>, usize), DiffError> {
        match side {
            DiffSide::Loaded(node, skip) => Ok((node, skip)),
            DiffSide::Id(id) => {
                if *nodes_read == limits.max_nodes {
                    return Err(DiffError::NodeLimitExceeded {
                        limit: limits.max_nodes,
                    });
                }
                *nodes_read += 1;
                Ok((self.read_at(id, position).await?, 0))
            }
        }
    }

    fn diff_emit(
        changes: &mut Vec<DiffEntry<C::Key, C::Value>>,
        entry: DiffEntry<C::Key, C::Value>,
        limits: DiffLimits,
    ) -> Result<(), DiffError> {
        if changes.len() == limits.max_changes {
            return Err(DiffError::ChangeLimitExceeded {
                limit: limits.max_changes,
            });
        }
        changes.push(entry);
        Ok(())
    }

    /// Authentically merges two radix roots and emits the unique canonical union shape.
    /// A key present on both sides keeps the value in `left`.
    ///
    /// Only the subtrees where the roots differ are read, and only the entries `right`
    /// adds are written, so two roots that grew apart from shared history cost what
    /// they differ by rather than what they hold.
    pub(crate) async fn union(&self, left: C::Id, right: C::Id) -> Result<C::Id, Error> {
        if left == right {
            return Ok(left);
        }
        let added = self
            .paired_diff(left, right, DiffLimits::unbounded())
            .await
            .map_err(DiffError::into_unbounded)?
            .into_iter()
            .filter_map(|entry| match entry {
                DiffEntry::Added(key, value) => Some((key, Some(value))),
                DiffEntry::Changed(..) | DiffEntry::Removed(..) => None,
            });
        self.update_batch(left, added).await
    }

    fn build_subtree<'b>(
        &'b self,
        position: usize,
        entries: &'b [(C::Key, C::Value)],
    ) -> BuildFuture<'b, C::Id> {
        Box::pin(async move {
            let first = entries[0].0.bytes();
            let last = entries[entries.len() - 1].0.bytes();
            let common = first[position..]
                .iter()
                .zip(&last[position..])
                .take_while(|(left, right)| left == right)
                .count();
            let end = position + common;
            let prefix = first[position..end].to_vec();
            if end == C::Key::LEN {
                return self
                    .write(Node {
                        prefix,
                        value: Some(entries[0].1),
                        children: Vec::new(),
                        entries: Vec::new(),
                    })
                    .await;
            }

            let mut children = Vec::new();
            let mut inline_entries = Vec::new();
            let mut start = 0;
            while start < entries.len() {
                let edge = entries[start].0.bytes()[end];
                let group_end =
                    start + entries[start..].partition_point(|(key, _)| key.bytes()[end] == edge);
                if group_end - start == 1 {
                    // Single-entry group: inline it instead of creating a child node.
                    let suffix = entries[start].0.bytes()[end..].to_vec();
                    inline_entries.push((suffix, entries[start].1));
                } else {
                    let child = self
                        .build_subtree(end + 1, &entries[start..group_end])
                        .await?;
                    children.push((edge, child));
                }
                start = group_end;
            }
            self.write(Node {
                prefix,
                value: None,
                children,
                entries: inline_entries,
            })
            .await
        })
    }

    pub(crate) async fn contains_node(&self, id: C::Id) -> Result<bool, Error> {
        Ok(self.objects.exists_many(&[id.into()]).await?[0])
    }
}

impl<S: ImmutableObjectStore, C: Collection> ContextWalk<'_, S, C> {
    pub(crate) fn prefix(&self) -> &[u8] {
        &self.key[..self.prefix_len]
    }

    /// Skip the subtree announced by the immediately preceding [`ContextWalkStep::Node`].
    pub(crate) fn prune(&mut self) {
        let frame = self
            .stack
            .last()
            .expect("context walk prune follows a node announcement");
        assert!(
            frame.announced,
            "context walk prune requires a node announcement"
        );
        self.stack.pop();
    }

    pub(crate) async fn next(&mut self) -> Result<Option<ContextWalkStep<C>>, Error> {
        let result = self.next_inner().await;
        if result.is_err() {
            self.stack.clear();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<ContextWalkStep<C>>, Error> {
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            let end = frame.position + frame.node.prefix.len();
            self.key[frame.position..end].copy_from_slice(&frame.node.prefix);
            self.prefix_len = end;

            if !frame.announced {
                frame.announced = true;
                return Ok(Some(ContextWalkStep::Node { id: frame.id }));
            }
            if !frame.yielded_value {
                frame.yielded_value = true;
                if let Some(value) = frame.node.value {
                    return Ok(Some(ContextWalkStep::Entry {
                        key: C::Key::from_bytes(&self.key),
                        value,
                    }));
                }
            }
            // Merge-iterate children and entries by discriminating byte.
            let next_child = frame.node.children.get(frame.child_index).map(|&(e, _)| e);
            let next_entry = frame
                .node
                .entries
                .get(frame.entry_index)
                .and_then(|(s, _)| s.first().copied());
            match (next_child, next_entry) {
                (Some(ce), Some(ee)) if ce < ee => {
                    let id = frame.node.children[frame.child_index].1;
                    frame.child_index += 1;
                    self.key[end] = ce;
                    let position = end + 1;
                    let node = self.store.read_at(id, position).await?;
                    self.stack.push(ContextFrame {
                        id,
                        node,
                        position,
                        child_index: 0,
                        entry_index: 0,
                        yielded_value: false,
                        announced: false,
                    });
                    continue;
                }
                (Some(_), Some(_)) => {
                    // Entry first — its discriminating byte < child edge.
                    let (suffix, value) = &frame.node.entries[frame.entry_index];
                    self.key[end..end + suffix.len()].copy_from_slice(suffix);
                    let value = *value;
                    frame.entry_index += 1;
                    return Ok(Some(ContextWalkStep::Entry {
                        key: C::Key::from_bytes(&self.key),
                        value,
                    }));
                }
                (Some(ce), None) => {
                    let id = frame.node.children[frame.child_index].1;
                    frame.child_index += 1;
                    self.key[end] = ce;
                    let position = end + 1;
                    let node = self.store.read_at(id, position).await?;
                    self.stack.push(ContextFrame {
                        id,
                        node,
                        position,
                        child_index: 0,
                        entry_index: 0,
                        yielded_value: false,
                        announced: false,
                    });
                    continue;
                }
                (None, Some(_)) => {
                    let (suffix, value) = &frame.node.entries[frame.entry_index];
                    self.key[end..end + suffix.len()].copy_from_slice(suffix);
                    let value = *value;
                    frame.entry_index += 1;
                    return Ok(Some(ContextWalkStep::Entry {
                        key: C::Key::from_bytes(&self.key),
                        value,
                    }));
                }
                (None, None) => {
                    let id = frame.id;
                    self.stack.pop();
                    self.prefix_len = end;
                    return Ok(Some(ContextWalkStep::Complete { id }));
                }
            }
        }
    }
}

impl<'a, S, L, R> PairedContextWalk<'a, S, L, R>
where
    S: ImmutableObjectStore,
    L: Collection,
    R: Collection<Key = L::Key>,
{
    pub(crate) fn prefix(&self) -> &[u8] {
        &self.key[..self.prefix_len]
    }

    pub(crate) fn prune(&mut self) {
        let frame = self
            .stack
            .last()
            .expect("paired context walk prune follows a node announcement");
        assert!(
            frame.prunable,
            "paired context walk prune must immediately follow a node announcement"
        );
        self.stack.pop();
    }

    pub(crate) fn mark_conflict(&mut self) {
        self.stack
            .last_mut()
            .expect("paired context walk conflict follows an entry")
            .conflict_free = false;
    }

    pub(crate) async fn next(&mut self) -> Result<Option<PairedContextWalkStep<L, R>>, DiffError> {
        let result = self.next_inner().await;
        if result.is_err() {
            self.stack.clear();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<PairedContextWalkStep<L, R>>, DiffError> {
        loop {
            let stack_depth = self.stack.len();
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            let end = frame.position + frame.left.prefix.len();
            self.key[frame.position..end].copy_from_slice(&frame.left.prefix);
            self.prefix_len = end;
            if !frame.aligned {
                // Align the right cursor to this left node's complete compressed prefix.
                // A right-only branch is ignored; a missing right path is carried as
                // `Absent` until the left terminal reports the semantic mismatch.
                let mut right = std::mem::replace(&mut frame.right, PairedRight::Absent);
                let mut position = frame.position;
                let mut left_offset = 0usize;
                while left_offset < frame.left.prefix.len() {
                    let byte = frame.left.prefix[left_offset];
                    match right {
                        PairedRight::Absent => break,
                        PairedRight::Present { id, node, skip } if skip < node.prefix.len() => {
                            if node.prefix[skip] != byte {
                                right = PairedRight::Absent;
                                break;
                            }
                            right = PairedRight::Present {
                                id,
                                node,
                                skip: skip + 1,
                            };
                        }
                        PairedRight::Present { node, .. } => {
                            let Ok(index) =
                                node.children.binary_search_by_key(&byte, |(edge, _)| *edge)
                            else {
                                right = PairedRight::Absent;
                                break;
                            };
                            let child = node.children[index].1;
                            if self.nodes_read == self.max_nodes {
                                return Err(DiffError::NodeLimitExceeded {
                                    limit: self.max_nodes,
                                });
                            }
                            let child_node = self
                                .right
                                .read_at(child, position + 1)
                                .await
                                .map_err(DiffError::from)?;
                            self.nodes_read += 1;
                            right = PairedRight::Present {
                                id: child,
                                node: child_node,
                                skip: 0,
                            };
                        }
                    }
                    left_offset += 1;
                    position += 1;
                }
                frame.right = right;
                frame.aligned = true;
            }
            if !frame.announced {
                frame.announced = true;
                frame.prunable = true;
                let (right, right_skip) = match &frame.right {
                    PairedRight::Present { id, skip, .. } => (Some(*id), *skip),
                    PairedRight::Absent => (None, 0),
                };
                return Ok(Some(PairedContextWalkStep::Node {
                    left: frame.left_id,
                    right,
                    right_skip,
                }));
            }
            frame.prunable = false;

            if !frame.yielded_value {
                frame.yielded_value = true;
                if let Some(left) = frame.left.value {
                    if self.entries_emitted == self.max_changes {
                        return Err(DiffError::ChangeLimitExceeded {
                            limit: self.max_changes,
                        });
                    }
                    self.entries_emitted += 1;
                    let right = match &frame.right {
                        PairedRight::Present { node, skip, .. }
                            if *skip == node.prefix.len()
                                && node.children.is_empty()
                                && node.entries.is_empty() =>
                        {
                            node.value
                        }
                        _ => None,
                    };
                    return Ok(Some(PairedContextWalkStep::Entry { left, right }));
                }
            }
            // Merge-iterate children and inline entries by discriminating byte.
            let next_child = frame.left.children.get(frame.child_index).map(|&(e, _)| e);
            let next_entry = frame
                .left
                .entries
                .get(frame.entry_index)
                .and_then(|(s, _)| s.first().copied());
            // Determine whether to yield an entry or descend into a child.
            let yield_entry = match (next_child, next_entry) {
                (Some(ce), Some(ee)) if ce < ee => false,
                (Some(_), Some(_)) => true, // ee < ce
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (None, None) => {
                    let left = frame.left_id;
                    let (right, right_skip) = match &frame.right {
                        PairedRight::Present { id, skip, .. } => (Some(*id), *skip),
                        PairedRight::Absent => (None, 0),
                    };
                    let conflict_free = frame.conflict_free;
                    self.stack.pop();
                    if !conflict_free && let Some(parent) = self.stack.last_mut() {
                        parent.conflict_free = false;
                    }
                    return Ok(Some(PairedContextWalkStep::Complete {
                        left,
                        right,
                        right_skip,
                        conflict_free,
                    }));
                }
            };
            if yield_entry {
                // Inline entry — yield it directly.
                let (suffix, left_value) = &frame.left.entries[frame.entry_index];
                self.key[end..end + suffix.len()].copy_from_slice(suffix);
                let left_value = *left_value;
                frame.entry_index += 1;
                if self.entries_emitted == self.max_changes {
                    return Err(DiffError::ChangeLimitExceeded {
                        limit: self.max_changes,
                    });
                }
                self.entries_emitted += 1;
                // Try to find matching right entry.
                let right_value = match &frame.right {
                    PairedRight::Present { node, skip, .. } if *skip == node.prefix.len() => {
                        // Right node fully aligned — check its entries.
                        node.entries
                            .binary_search_by(|(s, _)| s.as_slice().cmp(suffix.as_slice()))
                            .ok()
                            .map(|idx| node.entries[idx].1)
                    }
                    _ => None,
                };
                return Ok(Some(PairedContextWalkStep::Entry {
                    left: left_value,
                    right: right_value,
                }));
            }
            let &(edge, left_id) = &frame.left.children[frame.child_index];
            frame.child_index += 1;
            if stack_depth >= self.max_pending {
                return Err(DiffError::PendingLimitExceeded {
                    limit: self.max_pending,
                });
            }
            if self.nodes_read == self.max_nodes {
                return Err(DiffError::NodeLimitExceeded {
                    limit: self.max_nodes,
                });
            }
            let child_position = end + 1;
            let left = self
                .left
                .read_at(left_id, child_position)
                .await
                .map_err(DiffError::from)?;
            self.nodes_read += 1;
            let right = match &frame.right {
                PairedRight::Absent => PairedRight::Absent,
                PairedRight::Present { id, node, skip } if *skip < node.prefix.len() => {
                    if node.prefix[*skip] == edge {
                        PairedRight::Present {
                            id: *id,
                            node: node.clone(),
                            skip: *skip + 1,
                        }
                    } else {
                        PairedRight::Absent
                    }
                }
                PairedRight::Present { node, .. } => match node
                    .children
                    .binary_search_by_key(&edge, |(child_edge, _)| *child_edge)
                {
                    Ok(index) => {
                        let right_id = node.children[index].1;
                        if self.nodes_read == self.max_nodes {
                            return Err(DiffError::NodeLimitExceeded {
                                limit: self.max_nodes,
                            });
                        }
                        let right = self
                            .right
                            .read_at(right_id, child_position)
                            .await
                            .map_err(DiffError::from)?;
                        self.nodes_read += 1;
                        PairedRight::Present {
                            id: right_id,
                            node: right,
                            skip: 0,
                        }
                    }
                    Err(_) => PairedRight::Absent,
                },
            };
            self.key[end] = edge;
            self.stack.push(PairedContextFrame {
                left_id,
                left,
                right,
                position: child_position,
                child_index: 0,
                entry_index: 0,
                yielded_value: false,
                announced: false,
                prunable: false,
                aligned: false,
                conflict_free: true,
            });
        }
    }
}

impl<S: ImmutableObjectStore, C: Collection> Cursor<'_, S, C> {
    pub(crate) async fn next(&mut self) -> Result<Option<(C::Key, C::Value)>, Error> {
        let result = self.next_inner().await;
        if result.is_err() {
            self.stack.clear();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<(C::Key, C::Value)>, Error> {
        while let Some((node, position, child_index, entry_index, yielded)) = self.stack.last_mut()
        {
            let end = *position + node.prefix.len();
            self.key[*position..end].copy_from_slice(&node.prefix);
            if !*yielded {
                *yielded = true;
                if let Some(value) = node.value {
                    return Ok(Some((C::Key::from_bytes(&self.key), value)));
                }
            }
            // Merge-iterate children and entries in sorted order by discriminating byte.
            let next_child = node.children.get(*child_index).map(|&(edge, _)| edge);
            let next_entry = node
                .entries
                .get(*entry_index)
                .and_then(|(suffix, _)| suffix.first().copied());
            match (next_child, next_entry) {
                (Some(ce), Some(ee)) if ce < ee => {
                    let id = node.children[*child_index].1;
                    *child_index += 1;
                    self.key[end] = ce;
                    let child_position = end + 1;
                    let child = self.store.read_at(id, child_position).await?;
                    self.stack.push((child, child_position, 0, 0, false));
                }
                (Some(_ce), Some(_ee)) => {
                    // ee < ce (disjoint, and ce < ee was false above).
                    let (suffix, value) = &node.entries[*entry_index];
                    self.key[end..end + suffix.len()].copy_from_slice(suffix);
                    let value = *value;
                    *entry_index += 1;
                    return Ok(Some((C::Key::from_bytes(&self.key), value)));
                }
                (Some(ce), None) => {
                    let id = node.children[*child_index].1;
                    *child_index += 1;
                    self.key[end] = ce;
                    let child_position = end + 1;
                    let child = self.store.read_at(id, child_position).await?;
                    self.stack.push((child, child_position, 0, 0, false));
                }
                (None, Some(_)) => {
                    let (suffix, value) = &node.entries[*entry_index];
                    self.key[end..end + suffix.len()].copy_from_slice(suffix);
                    let value = *value;
                    *entry_index += 1;
                    return Ok(Some((C::Key::from_bytes(&self.key), value)));
                }
                (None, None) => {
                    self.stack.pop();
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod paired_context_tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use wheat_core::{ApplicationId, ChangeId, RevisionId};

    use super::*;
    use crate::{
        ObjectStoreAdapter, application_map::ApplicationCollection,
        revision_map::RevisionCollection,
    };

    fn change(first: u8, last: u8) -> ChangeId {
        let mut bytes = [0; 16];
        bytes[0] = first;
        bytes[15] = last;
        ChangeId::from_bytes(bytes)
    }

    #[tokio::test]
    async fn paired_context_walk_allows_extra_right_branches_and_compressed_prefixes() {
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "paired-subset");
        let applications = Store::<_, ApplicationCollection>::new(&objects);
        let revisions = Store::<_, RevisionCollection>::new(&objects);
        let app_a = ApplicationId::from_digest([1; 32]);
        let app_b = ApplicationId::from_digest([2; 32]);
        let revision_a = RevisionId::from_digest([11; 32]);
        let revision_b = RevisionId::from_digest([12; 32]);
        let extra = RevisionId::from_digest([13; 32]);
        let a = change(7, 1);
        let b = change(7, 254);
        let extra_key = change(3, 9);
        let left = applications
            .update_batch(
                applications.empty_root().await.unwrap(),
                [(a, Some(app_a)), (b, Some(app_b))],
            )
            .await
            .unwrap();
        let right = revisions
            .update_batch(
                revisions.empty_root().await.unwrap(),
                [
                    (a, Some(revision_a)),
                    (b, Some(revision_b)),
                    (extra_key, Some(extra)),
                ],
            )
            .await
            .unwrap();
        let mut walk = applications
            .paired_context_walk(left, revisions, right, DiffLimits::default())
            .await
            .unwrap();
        let mut entries = Vec::new();
        while let Some(step) = walk.next().await.unwrap() {
            if let PairedContextWalkStep::Entry { left, right } = step {
                entries.push((left, right));
            }
        }
        assert_eq!(
            entries,
            [(app_a, Some(revision_a)), (app_b, Some(revision_b))]
        );
    }

    #[tokio::test]
    async fn paired_context_walk_enforces_entry_limit() {
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "paired-limit");
        let applications = Store::<_, ApplicationCollection>::new(&objects);
        let revisions = Store::<_, RevisionCollection>::new(&objects);
        let key = change(7, 1);
        let left = applications
            .update_batch(
                applications.empty_root().await.unwrap(),
                [(key, Some(ApplicationId::from_digest([1; 32])))],
            )
            .await
            .unwrap();
        let right = revisions
            .update_batch(
                revisions.empty_root().await.unwrap(),
                [(key, Some(RevisionId::from_digest([1; 32])))],
            )
            .await
            .unwrap();
        let mut walk = applications
            .paired_context_walk(
                left,
                revisions,
                right,
                DiffLimits {
                    max_changes: 0,
                    ..DiffLimits::default()
                },
            )
            .await
            .unwrap();
        while !matches!(
            walk.next().await,
            Err(DiffError::ChangeLimitExceeded { limit: 0 })
        ) {}
    }

    #[tokio::test]
    async fn paired_context_walk_enforces_node_limit() {
        // Four entries: two under byte 1 (multi-entry group → child node) and
        // two under byte 2 (multi-entry group → child node). The constructor
        // reads both roots (nodes_read = 2), so max_nodes = 2 means the first
        // child read hits the limit.
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "paired-node-limit");
        let applications = Store::<_, ApplicationCollection>::new(&objects);
        let revisions = Store::<_, RevisionCollection>::new(&objects);
        let a = change(1, 0);
        let b = change(1, 1);
        let c = change(2, 0);
        let d = change(2, 1);
        let left = applications
            .update_batch(
                applications.empty_root().await.unwrap(),
                [
                    (a, Some(ApplicationId::from_digest([1; 32]))),
                    (b, Some(ApplicationId::from_digest([2; 32]))),
                    (c, Some(ApplicationId::from_digest([3; 32]))),
                    (d, Some(ApplicationId::from_digest([4; 32]))),
                ],
            )
            .await
            .unwrap();
        let right = revisions
            .update_batch(
                revisions.empty_root().await.unwrap(),
                [
                    (a, Some(RevisionId::from_digest([1; 32]))),
                    (b, Some(RevisionId::from_digest([2; 32]))),
                    (c, Some(RevisionId::from_digest([3; 32]))),
                    (d, Some(RevisionId::from_digest([4; 32]))),
                ],
            )
            .await
            .unwrap();
        let mut walk = applications
            .paired_context_walk(
                left,
                revisions,
                right,
                DiffLimits {
                    max_nodes: 2,
                    ..DiffLimits::default()
                },
            )
            .await
            .unwrap();
        // Drain until the node limit fires.
        while !matches!(
            walk.next().await,
            Err(DiffError::NodeLimitExceeded { limit: 2 })
        ) {}
    }

    #[tokio::test]
    async fn paired_context_walk_enforces_pending_limit() {
        // Four entries that produce two child nodes. Pushing a child frame
        // increases stack depth. With max_pending = 1, the root frame
        // (stack depth 1) already meets the limit so any child push is rejected.
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "paired-pending-limit");
        let applications = Store::<_, ApplicationCollection>::new(&objects);
        let revisions = Store::<_, RevisionCollection>::new(&objects);
        let a = change(1, 0);
        let b = change(1, 1);
        let c = change(2, 0);
        let d = change(2, 1);
        let left = applications
            .update_batch(
                applications.empty_root().await.unwrap(),
                [
                    (a, Some(ApplicationId::from_digest([1; 32]))),
                    (b, Some(ApplicationId::from_digest([2; 32]))),
                    (c, Some(ApplicationId::from_digest([3; 32]))),
                    (d, Some(ApplicationId::from_digest([4; 32]))),
                ],
            )
            .await
            .unwrap();
        let right = revisions
            .update_batch(
                revisions.empty_root().await.unwrap(),
                [
                    (a, Some(RevisionId::from_digest([1; 32]))),
                    (b, Some(RevisionId::from_digest([2; 32]))),
                    (c, Some(RevisionId::from_digest([3; 32]))),
                    (d, Some(RevisionId::from_digest([4; 32]))),
                ],
            )
            .await
            .unwrap();
        let mut walk = applications
            .paired_context_walk(
                left,
                revisions,
                right,
                DiffLimits {
                    max_pending: 1,
                    ..DiffLimits::default()
                },
            )
            .await
            .unwrap();
        while !matches!(
            walk.next().await,
            Err(DiffError::PendingLimitExceeded { limit: 1 })
        ) {}
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    fn node(prefix: usize, children: usize, entries: &[usize]) -> Node<u8, ()> {
        Node {
            prefix: vec![0; prefix],
            value: None,
            children: (0..children).map(|edge| (edge as u8, 0)).collect(),
            entries: entries
                .iter()
                .enumerate()
                .map(|(index, len)| {
                    let mut suffix = vec![0; *len];
                    if let Some(first) = suffix.first_mut() {
                        *first = 100 + index as u8;
                    }
                    (suffix, ())
                })
                .collect(),
        }
    }

    #[test]
    fn entries_must_hold_exactly_the_rest_of_the_key() {
        assert!(validate_node_at(&node(2, 0, &[6, 6]), 0, 8).is_ok());
        assert!(validate_node_at(&node(2, 1, &[5]), 1, 8).is_ok());
        for bad in [&[6, 5][..], &[6, 7], &[0, 6]] {
            assert!(matches!(
                validate_node_at(&node(2, 0, bad), 0, 8),
                Err(Error::InvalidChildDepth)
            ));
        }
    }

    #[test]
    fn nothing_branches_below_full_depth() {
        assert!(matches!(
            validate_node_at(&node(8, 2, &[]), 0, 8),
            Err(Error::InvalidChildDepth)
        ));
        assert!(matches!(
            validate_node_at(&node(7, 1, &[0]), 1, 8),
            Err(Error::InvalidChildDepth)
        ));
    }
}
