//! Shared implementation for the fixed-byte, copy-on-write radix collections.
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
    pub depth: u8,
    pub value: Option<V>,
    pub children: Vec<(u8, I)>,
}

pub(crate) trait Collection: Send + Sync {
    type Id: Copy + Eq + Into<ObjectId> + Send + Sync;
    type Key: Key;
    type Value: Copy + Eq + Send + Sync;
    type PublicNode: Send;

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

type Frame<C> = (Node<<C as Collection>::Id, <C as Collection>::Value>, usize);
type UpdateFuture<'a, I> = Pin<Box<dyn Future<Output = Result<Option<I>, Error>> + Send + 'a>>;

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
            depth: 0,
            value: None,
            children: vec![],
        })
        .await
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

    async fn read(&self, id: C::Id) -> Result<Node<C::Id, C::Value>, Error> {
        Ok(C::unpack(self.read_public(id).await?))
    }

    async fn root(&self, id: C::Id) -> Result<Node<C::Id, C::Value>, Error> {
        let node = self.read(id).await?;
        if node.depth != 0 {
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
            stack: vec![(node, 0)],
            key: vec![0; C::Key::LEN],
        })
    }

    pub(crate) async fn get(&self, root: C::Id, key: C::Key) -> Result<Option<C::Value>, Error> {
        let mut node = self.root(root).await?;
        loop {
            if usize::from(node.depth) == C::Key::LEN {
                return Ok(node.value);
            }
            let edge = key.bytes()[usize::from(node.depth)];
            let Ok(index) = node.children.binary_search_by_key(&edge, |child| child.0) else {
                return Ok(None);
            };
            let child = self.read(node.children[index].1).await?;
            if child.depth != node.depth + 1 {
                return Err(Error::InvalidChildDepth);
            }
            node = child;
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
        let node = self.root(root).await?;
        Ok(self
            .update_node(Some(root), node, &edits)
            .await?
            .expect("root is never pruned"))
    }

    fn update_node<'b>(
        &'b self,
        original: Option<C::Id>,
        mut node: Node<C::Id, C::Value>,
        edits: &'b [(C::Key, Option<C::Value>)],
    ) -> UpdateFuture<'b, C::Id> {
        Box::pin(async move {
            if edits.is_empty() {
                return Ok(original);
            }
            let mut changed = false;
            if usize::from(node.depth) == C::Key::LEN {
                changed = node.value != edits[0].1;
                node.value = edits[0].1;
            } else {
                let depth = usize::from(node.depth);
                let mut start = 0;
                while start < edits.len() {
                    let edge = edits[start].0.bytes()[depth];
                    let end = start
                        + edits[start..].partition_point(|(key, _)| key.bytes()[depth] == edge);
                    let position = node.children.binary_search_by_key(&edge, |child| child.0);
                    let old = position.ok().map(|i| node.children[i].1);
                    let child = if let Some(id) = old {
                        let child = self.read(id).await?;
                        if child.depth != node.depth + 1 {
                            return Err(Error::InvalidChildDepth);
                        }
                        child
                    } else {
                        Node {
                            depth: node.depth + 1,
                            value: None,
                            children: vec![],
                        }
                    };
                    let new = self.update_node(old, child, &edits[start..end]).await?;
                    if old != new {
                        changed = true;
                        match (position, new) {
                            (Ok(i), Some(id)) => node.children[i].1 = id,
                            (Ok(i), None) => {
                                node.children.remove(i);
                            }
                            (Err(i), Some(id)) => node.children.insert(i, (edge, id)),
                            (Err(_), None) => {}
                        }
                    }
                    start = end;
                }
            }
            if !changed {
                return Ok(original);
            }
            if node.depth != 0 && node.value.is_none() && node.children.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.write(node).await?))
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
        while let Some((node, index)) = self.stack.last_mut() {
            if usize::from(node.depth) == C::Key::LEN {
                let value = node.value;
                self.stack.pop();
                if let Some(value) = value {
                    return Ok(Some((C::Key::from_bytes(&self.key), value)));
                }
                continue;
            }
            let Some(&(edge, id)) = node.children.get(*index) else {
                self.stack.pop();
                continue;
            };
            *index += 1;
            self.key[usize::from(node.depth)] = edge;
            let child = self.store.read(id).await?;
            if child.depth != node.depth + 1 {
                return Err(Error::InvalidChildDepth);
            }
            self.stack.push((child, 0));
        }
        Ok(None)
    }
}
