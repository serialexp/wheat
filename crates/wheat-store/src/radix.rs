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
        let end = position
            .checked_add(node.prefix.len())
            .ok_or(Error::InvalidChildDepth)?;
        if end > C::Key::LEN {
            return Err(Error::InvalidChildDepth);
        }
        if node.value.is_some() {
            if end != C::Key::LEN || !node.children.is_empty() {
                return Err(Error::InvalidTerminalNode);
            }
        } else if node.children.len() == 1
            || (node.children.is_empty() && !(position == 0 && node.prefix.is_empty()))
        {
            return Err(Error::InvalidTerminalNode);
        }
        Ok(())
    }

    async fn root(&self, id: C::Id) -> Result<Node<C::Id, C::Value>, Error> {
        let node = self.read_at(id, 0).await?;
        if node.prefix.is_empty() && node.value.is_none() && node.children.len() == 1 {
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
            stack: vec![(node, 0, 0, false)],
            key: vec![0; C::Key::LEN],
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
            let edge = key.bytes()[end];
            let Ok(index) = node.children.binary_search_by_key(&edge, |child| child.0) else {
                return Ok(None);
            };
            position = end + 1;
            node = self.read_at(node.children[index].1, position).await?;
        }
    }

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
        let initially_empty =
            initial.prefix.is_empty() && initial.value.is_none() && initial.children.is_empty();
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
            let was_empty =
                node.prefix.is_empty() && node.value.is_none() && node.children.is_empty();
            if was_empty {
                let Some(value) = edit.1 else { continue };
                current = self
                    .write(Node {
                        prefix: edit.0.bytes().to_vec(),
                        value: Some(value),
                        children: Vec::new(),
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
            if common < node.prefix.len() {
                if edits.iter().all(|(_, value)| value.is_none()) {
                    return Ok(original);
                }
                let split = position + common;
                let old_edge = node.prefix[common];
                let old_child = self
                    .write(Node {
                        prefix: node.prefix[common + 1..].to_vec(),
                        value: node.value,
                        children: node.children,
                    })
                    .await?;
                let mut entries = BTreeMap::new();
                entries.insert(old_edge, old_child);
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
                        entries.insert(edge, self.build_subtree(split + 1, &present).await?);
                    }
                    start = group_end;
                }
                if entries.len() == 1 {
                    return Ok(original);
                }
                return Ok(Some(
                    self.write(Node {
                        prefix: node.prefix[..common].to_vec(),
                        value: None,
                        children: entries.into_iter().collect(),
                    })
                    .await?,
                ));
            }

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
                        })
                        .await?,
                    )),
                    None if position == 0 => Ok(Some(self.empty_root().await?)),
                    None => Ok(None),
                };
            }

            let mut children = node.children;
            let mut changed = false;
            let mut start = 0;
            while start < edits.len() {
                let edge = edits[start].0.bytes()[end];
                let group_end =
                    start + edits[start..].partition_point(|(key, _)| key.bytes()[end] == edge);
                let location = children.binary_search_by_key(&edge, |child| child.0);
                let old = location.ok().map(|index| children[index].1);
                let replacement = if let Some(id) = old {
                    let child = self.read_at(id, end + 1).await?;
                    self.update_node(Some(id), child, end + 1, &edits[start..group_end])
                        .await?
                } else {
                    let present: Vec<_> = edits[start..group_end]
                        .iter()
                        .filter_map(|(key, value)| value.map(|value| (*key, value)))
                        .collect();
                    if present.is_empty() {
                        None
                    } else {
                        Some(self.build_subtree(end + 1, &present).await?)
                    }
                };
                if replacement != old {
                    changed = true;
                    match (location, replacement) {
                        (Ok(index), Some(id)) => children[index].1 = id,
                        (Ok(index), None) => {
                            children.remove(index);
                        }
                        (Err(index), Some(id)) => children.insert(index, (edge, id)),
                        (Err(_), None) => {}
                    }
                }
                start = group_end;
            }
            if !changed {
                return Ok(original);
            }
            if children.is_empty() {
                return if position == 0 {
                    Ok(Some(self.empty_root().await?))
                } else {
                    Ok(None)
                };
            }
            if children.len() == 1 {
                let (edge, child_id) = children[0];
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
                    })
                    .await?,
                ));
            }
            Ok(Some(
                self.write(Node {
                    prefix: node.prefix,
                    value: None,
                    children,
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
                        let mut left_index = left.children.len();
                        let mut right_index = right.children.len();
                        while left_index != 0 || right_index != 0 {
                            let left_child = left_index.checked_sub(1).map(|i| left.children[i]);
                            let right_child = right_index.checked_sub(1).map(|i| right.children[i]);
                            match (left_child, right_child) {
                                (Some((le, li)), Some((re, ri))) if le == re => {
                                    left_index -= 1;
                                    right_index -= 1;
                                    if li != ri {
                                        pending.push(DiffTask::Pair(
                                            DiffSide::Id(li),
                                            DiffSide::Id(ri),
                                            end + 1,
                                            Some(le),
                                        ));
                                    }
                                }
                                (Some((le, li)), Some((re, _))) if le > re => {
                                    left_index -= 1;
                                    pending.push(DiffTask::One(
                                        DiffSide::Id(li),
                                        end + 1,
                                        Some(le),
                                        false,
                                    ));
                                }
                                (Some(_), Some((re, ri))) => {
                                    right_index -= 1;
                                    pending.push(DiffTask::One(
                                        DiffSide::Id(ri),
                                        end + 1,
                                        Some(re),
                                        true,
                                    ));
                                }
                                (Some((le, li)), None) => {
                                    left_index -= 1;
                                    pending.push(DiffTask::One(
                                        DiffSide::Id(li),
                                        end + 1,
                                        Some(le),
                                        false,
                                    ));
                                }
                                (None, Some((re, ri))) => {
                                    right_index -= 1;
                                    pending.push(DiffTask::One(
                                        DiffSide::Id(ri),
                                        end + 1,
                                        Some(re),
                                        true,
                                    ));
                                }
                                (None, None) => break,
                            }
                        }
                    } else if common == left_prefix.len() {
                        let end = position + common;
                        let edge = right_prefix[common];
                        let matching = left
                            .children
                            .binary_search_by_key(&edge, |child| child.0)
                            .ok();
                        let mut divergent = Some(right);
                        for (index, &(child_edge, id)) in left.children.iter().enumerate().rev() {
                            if matching.is_none() && child_edge < edge {
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
                        let mut divergent = Some(left);
                        for (index, &(child_edge, id)) in right.children.iter().enumerate().rev() {
                            if matching.is_none() && child_edge < edge {
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
    pub(crate) async fn union(&self, left: C::Id, right: C::Id) -> Result<C::Id, Error> {
        if left == right {
            return Ok(left);
        }
        let mut entries = BTreeMap::new();
        let mut left_cursor = self.cursor(left).await?;
        while let Some((key, value)) = left_cursor.next().await? {
            entries.insert(key, value);
        }
        let mut changed = false;
        let mut right_cursor = self.cursor(right).await?;
        while let Some((key, value)) = right_cursor.next().await? {
            if let std::collections::btree_map::Entry::Vacant(entry) = entries.entry(key) {
                entry.insert(value);
                changed = true;
            }
        }
        if !changed {
            return Ok(left);
        }
        let entries = entries.into_iter().collect::<Vec<_>>();
        self.build_subtree(0, &entries).await
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
                    })
                    .await;
            }

            let mut children = Vec::new();
            let mut start = 0;
            while start < entries.len() {
                let edge = entries[start].0.bytes()[end];
                let group_end =
                    start + entries[start..].partition_point(|(key, _)| key.bytes()[end] == edge);
                let child = self
                    .build_subtree(end + 1, &entries[start..group_end])
                    .await?;
                children.push((edge, child));
                start = group_end;
            }
            self.write(Node {
                prefix,
                value: None,
                children,
            })
            .await
        })
    }

    pub(crate) async fn contains_node(&self, id: C::Id) -> Result<bool, Error> {
        Ok(self.objects.exists_many(&[id.into()]).await?[0])
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
        while let Some((node, position, child_index, yielded)) = self.stack.last_mut() {
            let end = *position + node.prefix.len();
            self.key[*position..end].copy_from_slice(&node.prefix);
            if !*yielded {
                *yielded = true;
                if let Some(value) = node.value {
                    return Ok(Some((C::Key::from_bytes(&self.key), value)));
                }
            }
            let Some(&(edge, id)) = node.children.get(*child_index) else {
                self.stack.pop();
                continue;
            };
            *child_index += 1;
            self.key[end] = edge;
            let child_position = end + 1;
            let child = self.store.read_at(id, child_position).await?;
            self.stack.push((child, child_position, 0, false));
        }
        Ok(None)
    }
}
