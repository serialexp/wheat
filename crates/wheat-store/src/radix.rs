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

/// Checks a node's shape at key byte `position`, for keys of `key_len` bytes in a
/// collection whose buckets hold up to `bucket` entries: the prefix stays within the
/// key, children and inline entries end exactly at full depth, a value only sits on
/// a terminal root at full depth (a single entry below the root is inlined in its
/// parent), an interior node branches on at least two bytes (only an empty root may
/// be bare), and a node below the root without children holds more than one bucket
/// (a smaller group is inlined in its parent). With the bucket and edge checks done
/// at decoding, this leaves one encoding per set of entries.
pub(crate) fn validate_node_at<I, V>(
    node: &Node<I, V>,
    position: usize,
    key_len: usize,
    bucket: usize,
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
    // Entries are sorted, so a bucket's entries are adjacent.
    let entry_bytes = node
        .entries
        .iter()
        .enumerate()
        .filter(|(index, (suffix, _))| {
            *index == 0 || node.entries[index - 1].0.first() != suffix.first()
        })
        .count();
    let total_branches = node.children.len() + entry_bytes;
    if node.value.is_some() {
        // Terminal: the root, at full key depth, no children or entries.
        if position != 0 || end != key_len || total_branches != 0 {
            return Err(Error::InvalidTerminalNode);
        }
    } else if total_branches == 1
        || (total_branches == 0 && !(position == 0 && node.prefix.is_empty()))
        || (position != 0 && node.children.is_empty() && node.entries.len() <= bucket)
    {
        return Err(Error::InvalidTerminalNode);
    }
    Ok(())
}

pub(crate) trait Collection: Send + Sync {
    type Id: Copy + Eq + Into<ObjectId> + Send + Sync;
    type Key: Key;
    type Value: Clone + Eq + Send + Sync;
    type PublicNode: Clone + Send;

    const KIND: ObjectKind;
    /// The most entries sharing their next key byte that stay inline in a node; a
    /// larger group becomes a child. At most 2: a diff pairs a bucket with a child
    /// as one synthetic node, which two entries always split from cleanly.
    const BUCKET: usize = 1;
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
type UpdateFuture<'a, I, V> =
    Pin<Box<dyn Future<Output = Result<Updated<I, V>, Error>> + Send + 'a>>;

/// What [`Store::update_node`] made of a subtree. At the root it is always `Node`.
enum Updated<I, V> {
    /// The subtree's node; the original one when nothing changed.
    Node(I),
    /// At most one bucket of entries is left: their key bytes from the subtree's
    /// position on, in key order. The parent inlines them, because a canonical child
    /// holds more than a bucket.
    Bucket(Vec<(Vec<u8>, V)>),
    /// No entries are left.
    Pruned,
}

/// A child of a node being updated: stored, or split off the node's prefix and not
/// written yet.
enum Child<I, V> {
    Stored(I),
    Detached(Node<I, V>),
}

/// The children of a node being updated, by edge byte.
type Children<C> = Vec<(u8, Child<<C as Collection>::Id, <C as Collection>::Value>)>;
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
    /// Entry diffs, in key order, that sort between their node's queued subtrees.
    Emit(Vec<DiffEntry<C::Key, C::Value>>),
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
        const { assert!(C::BUCKET == 1 || C::BUCKET == 2) };
        validate_node_at(node, position, C::Key::LEN, C::BUCKET)
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
                return Ok(Some(node.entries[index].1.clone()));
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
                .filter_map(|(key, value)| value.clone().map(|value| (*key, value)))
                .collect::<Vec<_>>();
            return if present.is_empty() {
                Ok(root)
            } else {
                self.build_subtree(0, &present).await
            };
        }
        // One pass for the whole batch: each node on a changed path is written once.
        match self.update_node(Some(root), initial, 0, &edits).await? {
            Updated::Node(id) => Ok(id),
            Updated::Bucket(..) | Updated::Pruned => {
                unreachable!("update_node writes the root itself")
            }
        }
    }

    /// Entries that replace a branch at byte `end` of their keys, all sharing that
    /// byte and in key order: none drops it, up to a bucket are inlined, more become a
    /// child.
    async fn place(
        &self,
        end: usize,
        present: Vec<(C::Key, C::Value)>,
        children: &mut Children<C>,
        entries: &mut Vec<(Vec<u8>, C::Value)>,
    ) -> Result<(), Error> {
        if present.is_empty() {
            return Ok(());
        }
        let edge = present[0].0.bytes()[end];
        if present.len() <= C::BUCKET {
            let index = entries.partition_point(|(entry, _)| entry[0] < edge);
            entries.splice(
                index..index,
                present
                    .into_iter()
                    .map(|(key, value)| (key.bytes()[end..].to_vec(), value)),
            );
        } else {
            let child = self.build_subtree(end + 1, &present).await?;
            let index = children
                .binary_search_by_key(&edge, |(edge, _)| *edge)
                .unwrap_err();
            children.insert(index, (edge, Child::Stored(child)));
        }
        Ok(())
    }

    /// At most a bucket of entries left at `position`, as key bytes from there on: the
    /// root is rebuilt from them, any other subtree hands them to its parent to
    /// inline, so nothing non-canonical is written.
    async fn bucket(
        &self,
        position: usize,
        rest: Vec<(Vec<u8>, C::Value)>,
    ) -> Result<Updated<C::Id, C::Value>, Error> {
        if position != 0 {
            return Ok(Updated::Bucket(rest));
        }
        let present: Vec<_> = rest
            .into_iter()
            .map(|(key, value)| (C::Key::from_bytes(&key), value))
            .collect();
        Ok(Updated::Node(self.build_subtree(0, &present).await?))
    }

    /// No entries left at `position`: the root becomes the empty root.
    async fn pruned(&self, position: usize) -> Result<Updated<C::Id, C::Value>, Error> {
        if position != 0 {
            return Ok(Updated::Pruned);
        }
        Ok(Updated::Node(self.empty_root().await?))
    }

    /// Recursively update a node with a batch of sorted, distinct edits, producing
    /// canonical inline-entry structure. `original` is the node's ID, or `None` for
    /// a node split off a prefix and not written yet.
    fn update_node<'b>(
        &'b self,
        original: Option<C::Id>,
        node: Node<C::Id, C::Value>,
        position: usize,
        edits: &'b [(C::Key, Option<C::Value>)],
    ) -> UpdateFuture<'b, C::Id, C::Value> {
        Box::pin(async move {
            // A terminal node is a root holding one entry: rebuild from it and the edits.
            if let Some(value) = node.value {
                let mut key = edits
                    .first()
                    .map(|(key, _)| key.bytes()[..position].to_vec())
                    .unwrap_or_default();
                key.extend_from_slice(&node.prefix);
                let key = C::Key::from_bytes(&key);
                let mut present = BTreeMap::from([(key, value.clone())]);
                for (edit, value) in edits {
                    match value {
                        Some(value) => present.insert(*edit, value.clone()),
                        None => present.remove(edit),
                    };
                }
                if let Some(original) = original
                    && present.len() == 1
                    && present.get(&key) == Some(&value)
                {
                    return Ok(Updated::Node(original));
                }
                return if present.is_empty() {
                    self.pruned(position).await
                } else if present.len() <= C::BUCKET {
                    let rest = present
                        .into_iter()
                        .map(|(key, value)| (key.bytes()[position..].to_vec(), value))
                        .collect();
                    self.bucket(position, rest).await
                } else {
                    let present: Vec<_> = present.into_iter().collect();
                    Ok(Updated::Node(self.build_subtree(position, &present).await?))
                };
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

            // Edits that leave the prefix early split it: the old node becomes a
            // child under its next prefix byte, kept in memory so edits below that
            // byte update it before anything is written.
            let (mut prefix, mut children, mut entries) = if common < node.prefix.len() {
                let detached = Node {
                    prefix: node.prefix[common + 1..].to_vec(),
                    value: None,
                    children: node.children,
                    entries: node.entries,
                };
                let mut prefix = node.prefix;
                let edge = prefix[common];
                prefix.truncate(common);
                (prefix, vec![(edge, Child::Detached(detached))], Vec::new())
            } else {
                (
                    node.prefix,
                    node.children
                        .into_iter()
                        .map(|(edge, id)| (edge, Child::Stored(id)))
                        .collect::<Vec<_>>(),
                    node.entries,
                )
            };
            let end = position + prefix.len();

            let mut changed = false;
            let mut start = 0;
            while start < edits.len() {
                let edge = edits[start].0.bytes()[end];
                let group_end =
                    start + edits[start..].partition_point(|(key, _)| key.bytes()[end] == edge);
                let group = &edits[start..group_end];
                start = group_end;

                if let Ok(index) = children.binary_search_by_key(&edge, |(edge, _)| *edge) {
                    let (_, child) = children.remove(index);
                    let (child_original, child) = match child {
                        Child::Stored(id) => (Some(id), self.read_at(id, end + 1).await?),
                        Child::Detached(node) => (None, node),
                    };
                    match self
                        .update_node(child_original, child, end + 1, group)
                        .await?
                    {
                        Updated::Node(id) => {
                            changed |= Some(id) != child_original;
                            children.insert(index, (edge, Child::Stored(id)));
                        }
                        Updated::Bucket(rest) => {
                            // A bucket left below this edge: inline it.
                            changed = true;
                            let index = entries.partition_point(|(entry, _)| entry[0] < edge);
                            entries.splice(
                                index..index,
                                rest.into_iter().map(|(rest, value)| {
                                    let mut suffix = Vec::with_capacity(1 + rest.len());
                                    suffix.push(edge);
                                    suffix.extend_from_slice(&rest);
                                    (suffix, value)
                                }),
                            );
                        }
                        Updated::Pruned => changed = true,
                    }
                } else {
                    // The group meets the inline bucket at this byte, if any: merge them.
                    let first = entries.partition_point(|(entry, _)| entry[0] < edge);
                    let last =
                        first + entries[first..].partition_point(|(entry, _)| entry[0] == edge);
                    let bucket: Vec<_> = entries.drain(first..last).collect();
                    let mut present = BTreeMap::new();
                    for (suffix, value) in &bucket {
                        let mut key = group[0].0.bytes()[..end].to_vec();
                        key.extend_from_slice(suffix);
                        present.insert(C::Key::from_bytes(&key), value.clone());
                    }
                    for (edit, value) in group {
                        match value {
                            Some(value) => present.insert(*edit, value.clone()),
                            None => present.remove(edit),
                        };
                    }
                    if present.len() == bucket.len()
                        && present
                            .iter()
                            .zip(&bucket)
                            .all(|((key, value), (suffix, old))| {
                                key.bytes()[end..] == suffix[..] && value == old
                            })
                    {
                        entries.splice(first..first, bucket);
                        continue;
                    }
                    changed = true;
                    self.place(
                        end,
                        present.into_iter().collect(),
                        &mut children,
                        &mut entries,
                    )
                    .await?;
                }
            }
            if !changed && let Some(original) = original {
                return Ok(Updated::Node(original));
            }

            match (children.len(), entries.len()) {
                (0, 0) => self.pruned(position).await,
                (0, count) if count <= C::BUCKET => {
                    // At most a bucket left: this subtree is now those entries.
                    let rest = entries
                        .into_iter()
                        .map(|(suffix, value)| ([prefix.as_slice(), &suffix].concat(), value))
                        .collect();
                    self.bucket(position, rest).await
                }
                (1, 0) => {
                    // A single child left: merge it into this node.
                    let (edge, child) = children.pop().expect("one child");
                    let child = match child {
                        Child::Stored(id) => self.read_at(id, end + 1).await?,
                        Child::Detached(node) => node,
                    };
                    prefix.reserve(1 + child.prefix.len());
                    prefix.push(edge);
                    prefix.extend_from_slice(&child.prefix);
                    Ok(Updated::Node(
                        self.write(Node {
                            prefix,
                            value: None,
                            children: child.children,
                            entries: child.entries,
                        })
                        .await?,
                    ))
                }
                _ => {
                    let mut stored = Vec::with_capacity(children.len());
                    for (edge, child) in children {
                        let id = match child {
                            Child::Stored(id) => id,
                            Child::Detached(node) => self.write(node).await?,
                        };
                        stored.push((edge, id));
                    }
                    Ok(Updated::Node(
                        self.write(Node {
                            prefix,
                            value: None,
                            children: stored,
                            entries,
                        })
                        .await?,
                    ))
                }
            }
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
                    let (mut node, skip) = self
                        .diff_load(side, position, &mut nodes_read, limits)
                        .await?;
                    let end = position + node.prefix.len() - skip;
                    key[position..end].copy_from_slice(&node.prefix[skip..]);
                    if let Some(value) = node.value.take() {
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
                    let branches = node_branches(&node);
                    let (left, right) = if added {
                        (Vec::new(), branches)
                    } else {
                        (branches, Vec::new())
                    };
                    Self::diff_branches(
                        &mut key,
                        end,
                        (left, &node.entries),
                        (right, &node.entries),
                        &mut pending,
                        &mut changes,
                        limits,
                    )?;
                }
                DiffTask::Emit(entries) => {
                    for entry in entries {
                        Self::diff_emit(&mut changes, entry, limits)?;
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
                        match (&left.value, &right.value) {
                            (Some(old), Some(new)) if old != new => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Changed(
                                    C::Key::from_bytes(&key),
                                    old.clone(),
                                    new.clone(),
                                ),
                                limits,
                            )?,
                            (Some(old), None) => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Removed(C::Key::from_bytes(&key), old.clone()),
                                limits,
                            )?,
                            (None, Some(new)) => Self::diff_emit(
                                &mut changes,
                                DiffEntry::Added(C::Key::from_bytes(&key), new.clone()),
                                limits,
                            )?,
                            _ => {}
                        }
                        let left_branches = node_branches(&left);
                        let right_branches = node_branches(&right);
                        Self::diff_branches(
                            &mut key,
                            end,
                            (left_branches, &left.entries),
                            (right_branches, &right.entries),
                            &mut pending,
                            &mut changes,
                            limits,
                        )?;
                    } else if common == left_prefix.len() {
                        // The right's prefix runs past the left's: from the left's end
                        // the right is one branch, on its next prefix byte.
                        let end = position + common;
                        let divergent = Branch::Subtree(
                            right_prefix[common],
                            DiffSide::Loaded(right, right_skip + common + 1),
                        );
                        Self::diff_branches(
                            &mut key,
                            end,
                            (node_branches(&left), &left.entries),
                            (vec![divergent], &[]),
                            &mut pending,
                            &mut changes,
                            limits,
                        )?;
                    } else {
                        let end = position + common;
                        let divergent = Branch::Subtree(
                            left_prefix[common],
                            DiffSide::Loaded(left, left_skip + common + 1),
                        );
                        Self::diff_branches(
                            &mut key,
                            end,
                            (vec![divergent], &[]),
                            (node_branches(&right), &right.entries),
                            &mut pending,
                            &mut changes,
                            limits,
                        )?;
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

    /// Diffs the branches below key byte `end` of two nodes, each given as its
    /// branches and the inline entries its buckets index. A byte can hold a subtree
    /// on one side and a bucket on the other; the bucket then pairs with the subtree
    /// as the node it would be.
    ///
    /// Branches are handled high to low, so the subtrees they queue pop low to high.
    /// Entry diffs wait until the next subtree is queued, then go in front of it as
    /// one [`DiffTask::Emit`]; those below every subtree are emitted at once. The
    /// diff so stays in key order.
    fn diff_branches(
        key: &mut [u8],
        end: usize,
        (mut left, left_entries): SideBranches<'_, C>,
        (mut right, right_entries): SideBranches<'_, C>,
        pending: &mut Vec<DiffTask<C>>,
        changes: &mut Vec<DiffEntry<C::Key, C::Value>>,
        limits: DiffLimits,
    ) -> Result<(), DiffError> {
        use std::cmp::Ordering;
        // Highest key first.
        let mut deferred = Vec::new();
        let mut bucket = Vec::new();
        loop {
            let order = match (left.last(), right.last()) {
                (None, None) => break,
                (Some(l), Some(r)) => l.byte().cmp(&r.byte()),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
            };
            let (l, r) = match order {
                Ordering::Equal => (left.pop(), right.pop()),
                Ordering::Greater => (left.pop(), None),
                Ordering::Less => (None, right.pop()),
            };
            let byte = l
                .as_ref()
                .or(r.as_ref())
                .expect("one side has the branch")
                .byte();
            let task = match (l, r) {
                (Some(Branch::Subtree(_, l)), Some(Branch::Subtree(_, r))) => {
                    if let (DiffSide::Id(l), DiffSide::Id(r)) = (&l, &r)
                        && l == r
                    {
                        continue;
                    }
                    DiffTask::Pair(l, r, end + 1, Some(byte))
                }
                (Some(Branch::Subtree(_, l)), Some(Branch::Bucket(_, r))) => DiffTask::Pair(
                    l,
                    DiffSide::Loaded(synthetic(&right_entries[r]), 0),
                    end + 1,
                    Some(byte),
                ),
                (Some(Branch::Bucket(_, l)), Some(Branch::Subtree(_, r))) => DiffTask::Pair(
                    DiffSide::Loaded(synthetic(&left_entries[l]), 0),
                    r,
                    end + 1,
                    Some(byte),
                ),
                (Some(Branch::Subtree(_, l)), None) => DiffTask::One(l, end + 1, Some(byte), false),
                (None, Some(Branch::Subtree(_, r))) => DiffTask::One(r, end + 1, Some(byte), true),
                (Some(Branch::Bucket(_, l)), Some(Branch::Bucket(_, r))) => {
                    diff_buckets::<C>(key, end, &left_entries[l], &right_entries[r], &mut bucket);
                    deferred.extend(bucket.drain(..).rev());
                    continue;
                }
                (Some(Branch::Bucket(_, l)), None) => {
                    for (suffix, value) in left_entries[l].iter().rev() {
                        key[end..end + suffix.len()].copy_from_slice(suffix);
                        deferred.push(DiffEntry::Removed(C::Key::from_bytes(key), value.clone()));
                    }
                    continue;
                }
                (None, Some(Branch::Bucket(_, r))) => {
                    for (suffix, value) in right_entries[r].iter().rev() {
                        key[end..end + suffix.len()].copy_from_slice(suffix);
                        deferred.push(DiffEntry::Added(C::Key::from_bytes(key), value.clone()));
                    }
                    continue;
                }
                (None, None) => unreachable!("one side has the branch"),
            };
            if !deferred.is_empty() {
                let mut run = std::mem::take(&mut deferred);
                run.reverse();
                pending.push(DiffTask::Emit(run));
            }
            pending.push(task);
        }
        for entry in deferred.into_iter().rev() {
            Self::diff_emit(changes, entry, limits)?;
        }
        Ok(())
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
                        value: Some(entries[0].1.clone()),
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
                if group_end - start <= C::BUCKET {
                    // A small group stays inline instead of becoming a child node.
                    for (key, value) in &entries[start..group_end] {
                        inline_entries.push((key.bytes()[end..].to_vec(), value.clone()));
                    }
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

/// One branch of a node being diffed, by its byte: a subtree, or the range of a
/// bucket of the node's inline entries.
enum Branch<C: Collection> {
    Subtree(u8, DiffSide<C>),
    Bucket(u8, std::ops::Range<usize>),
}

impl<C: Collection> Branch<C> {
    fn byte(&self) -> u8 {
        match self {
            Branch::Subtree(byte, _) | Branch::Bucket(byte, _) => *byte,
        }
    }
}

/// One side's branches below a byte, with the inline entries its buckets index.
type SideBranches<'n, C> = (Vec<Branch<C>>, &'n [(Vec<u8>, <C as Collection>::Value)]);

/// A node's branches in byte order: its children, and its inline entries grouped
/// into buckets by their first byte. Children and buckets never share a byte.
fn node_branches<C: Collection>(node: &Node<C::Id, C::Value>) -> Vec<Branch<C>> {
    let mut branches = Vec::with_capacity(node.children.len() + node.entries.len());
    let mut child = 0;
    let mut entry = 0;
    while child < node.children.len() || entry < node.entries.len() {
        let entry_byte = node.entries.get(entry).map(|(suffix, _)| suffix[0]);
        match node.children.get(child) {
            Some(&(edge, id)) if entry_byte.is_none_or(|byte| edge < byte) => {
                branches.push(Branch::Subtree(edge, DiffSide::Id(id)));
                child += 1;
            }
            _ => {
                let byte = entry_byte.expect("an entry is left");
                let range = bucket_range(&node.entries[entry..], byte);
                let range = entry + range.start..entry + range.end;
                entry = range.end;
                branches.push(Branch::Bucket(byte, range));
            }
        }
    }
    branches
}

/// The entries whose suffix starts with `byte`; entries are sorted by suffix.
fn bucket_range<V>(entries: &[(Vec<u8>, V)], byte: u8) -> std::ops::Range<usize> {
    let start = entries.partition_point(|(suffix, _)| suffix[0] < byte);
    start..start + entries[start..].partition_point(|(suffix, _)| suffix[0] == byte)
}

/// A bucket of one or two entries as the node a child would be, one byte below them:
/// one entry is a terminal value, two split right after their common prefix.
fn synthetic<I, V: Clone>(bucket: &[(Vec<u8>, V)]) -> Node<I, V> {
    match bucket {
        [(suffix, value)] => Node {
            prefix: suffix[1..].to_vec(),
            value: Some(value.clone()),
            children: Vec::new(),
            entries: Vec::new(),
        },
        [(first, first_value), (second, second_value)] => {
            let common = first[1..]
                .iter()
                .zip(&second[1..])
                .take_while(|(left, right)| left == right)
                .count();
            Node {
                prefix: first[1..1 + common].to_vec(),
                value: None,
                children: Vec::new(),
                entries: vec![
                    (first[1 + common..].to_vec(), first_value.clone()),
                    (second[1 + common..].to_vec(), second_value.clone()),
                ],
            }
        }
        _ => unreachable!("a bucket holds one or two entries"),
    }
}

/// The differences between two buckets on the same byte, in key order.
fn diff_buckets<C: Collection>(
    key: &mut [u8],
    end: usize,
    left: &[(Vec<u8>, C::Value)],
    right: &[(Vec<u8>, C::Value)],
    out: &mut Vec<DiffEntry<C::Key, C::Value>>,
) {
    let (mut l, mut r) = (0, 0);
    while l < left.len() || r < right.len() {
        let order = match (left.get(l), right.get(r)) {
            (Some((ls, _)), Some((rs, _))) => ls.cmp(rs),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, _) => std::cmp::Ordering::Greater,
        };
        match order {
            std::cmp::Ordering::Equal => {
                let ((suffix, old), (_, new)) = (&left[l], &right[r]);
                if old != new {
                    key[end..end + suffix.len()].copy_from_slice(suffix);
                    out.push(DiffEntry::Changed(
                        C::Key::from_bytes(key),
                        old.clone(),
                        new.clone(),
                    ));
                }
                l += 1;
                r += 1;
            }
            std::cmp::Ordering::Less => {
                let (suffix, old) = &left[l];
                key[end..end + suffix.len()].copy_from_slice(suffix);
                out.push(DiffEntry::Removed(C::Key::from_bytes(key), old.clone()));
                l += 1;
            }
            std::cmp::Ordering::Greater => {
                let (suffix, new) = &right[r];
                key[end..end + suffix.len()].copy_from_slice(suffix);
                out.push(DiffEntry::Added(C::Key::from_bytes(key), new.clone()));
                r += 1;
            }
        }
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
                if let Some(value) = frame.node.value.clone() {
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
                    let value = value.clone();
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
                    let value = value.clone();
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
                if let Some(left) = frame.left.value.clone() {
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
                            node.value.clone()
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
                let left_value = left_value.clone();
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
                            .map(|idx| node.entries[idx].1.clone())
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
                if let Some(value) = node.value.clone() {
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
                    let value = value.clone();
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
                    let value = value.clone();
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
    async fn paired_diff_lists_a_child_before_a_later_inline_entry() {
        // The root holds a child on byte 1 (two keys) and an inline entry on byte 9:
        // the child's change sorts first, though the entry is on the root itself.
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "paired-diff-order");
        let revisions = Store::<_, RevisionCollection>::new(&objects);
        let (low, sibling, high) = (change(1, 1), change(1, 2), change(9, 0));
        let revision = |n| RevisionId::from_digest([n; 32]);
        let empty = revisions.empty_root().await.unwrap();
        let left = revisions
            .update_batch(
                empty,
                [
                    (low, Some(revision(1))),
                    (sibling, Some(revision(2))),
                    (high, Some(revision(3))),
                ],
            )
            .await
            .unwrap();
        let right = revisions
            .update_batch(left, [(low, Some(revision(4))), (high, Some(revision(5)))])
            .await
            .unwrap();
        assert_eq!(
            revisions
                .paired_diff(left, right, DiffLimits::unbounded())
                .await
                .unwrap(),
            [
                DiffEntry::Changed(low, revision(1), revision(4)),
                DiffEntry::Changed(high, revision(3), revision(5)),
            ]
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
        assert!(validate_node_at(&node(2, 0, &[6, 6]), 0, 8, 1).is_ok());
        assert!(validate_node_at(&node(2, 1, &[5]), 1, 8, 1).is_ok());
        for bad in [&[6, 5][..], &[6, 7], &[0, 6]] {
            assert!(matches!(
                validate_node_at(&node(2, 0, bad), 0, 8, 1),
                Err(Error::InvalidChildDepth)
            ));
        }
    }

    #[test]
    fn a_childless_node_below_the_root_holds_more_than_one_bucket() {
        // Two single-entry branches: a node for buckets of one, inlined for two.
        assert!(validate_node_at(&node(2, 0, &[5, 5]), 1, 8, 1).is_ok());
        assert!(matches!(
            validate_node_at(&node(2, 0, &[5, 5]), 1, 8, 2),
            Err(Error::InvalidTerminalNode)
        ));
        assert!(validate_node_at(&node(2, 0, &[5, 5, 5]), 1, 8, 2).is_ok());
        // The root and nodes with children are exempt.
        assert!(validate_node_at(&node(2, 0, &[6, 6]), 0, 8, 2).is_ok());
        assert!(validate_node_at(&node(2, 1, &[5]), 1, 8, 2).is_ok());
    }

    #[test]
    fn nothing_branches_below_full_depth() {
        assert!(matches!(
            validate_node_at(&node(8, 2, &[]), 0, 8, 1),
            Err(Error::InvalidChildDepth)
        ));
        assert!(matches!(
            validate_node_at(&node(7, 1, &[0]), 1, 8, 1),
            Err(Error::InvalidChildDepth)
        ));
    }
}
