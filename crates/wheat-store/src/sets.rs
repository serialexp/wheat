use thiserror::Error;
use wheat_core::{
    ConflictId, ConflictSetChild, ConflictSetId, ConflictSetNode, ContentId, ContentSetChild,
    ContentSetId, ContentSetNode, ObjectId, RetainedSetChild, RetainedSetId, RetainedSetNode,
    decode_conflict_set_node, decode_content_set_node, decode_retained_set_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

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
        struct $collection;
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
set_collection!(
    ContentCollection,
    ContentSetId,
    ContentId,
    ContentSetNode,
    ContentSetChild,
    ContentSetNode,
    decode_content_set_node
);
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
set_api!(
    ContentSetStore,
    ContentSetCursor,
    ContentCollection,
    ContentSetId,
    ContentId,
    ContentSetNode
);
set_api!(
    RetainedSetStore,
    RetainedSetCursor,
    RetainedCollection,
    RetainedSetId,
    ObjectId,
    RetainedSetNode
);
