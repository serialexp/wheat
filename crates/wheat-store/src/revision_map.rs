use thiserror::Error;
use wheat_core::{
    ChangeId, RevisionId, RevisionMapChild, RevisionMapId, RevisionMapNode,
    decode_revision_map_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

#[derive(Debug, Error)]
pub enum RevisionMapError {
    #[error("immutable revision-map storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("revision-map node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("revision-map root must have depth zero")]
    InvalidRootDepth,
    #[error("revision-map child depth does not match its parent")]
    InvalidChildDepth,
    #[error("revision-map node has an invalid terminal shape")]
    InvalidTerminalNode,
}

impl From<radix::Error> for RevisionMapError {
    fn from(error: radix::Error) -> Self {
        match error {
            radix::Error::Store(e) => Self::Store(e),
            radix::Error::Decode(e) => Self::Decode(e),
            radix::Error::InvalidRootDepth => Self::InvalidRootDepth,
            radix::Error::InvalidChildDepth => Self::InvalidChildDepth,
            radix::Error::InvalidTerminalNode => Self::InvalidTerminalNode,
        }
    }
}

struct RevisionCollection;
impl radix::Key for ChangeId {
    const LEN: usize = 16;
    fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }
    fn from_bytes(bytes: &[u8]) -> Self {
        Self::from_bytes(bytes.try_into().expect("radix key length is fixed"))
    }
}
impl radix::Collection for RevisionCollection {
    type Id = RevisionMapId;
    type Key = ChangeId;
    type Value = RevisionId;
    type PublicNode = RevisionMapNode;
    const KIND: ObjectKind = ObjectKind::RevisionMapNode;
    fn decode(bytes: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_revision_map_node(bytes)
    }
    fn unpack(node: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
        radix::Node {
            prefix: node.prefix,
            value: node.value,
            children: node
                .children
                .into_iter()
                .map(|c| (c.edge, c.child))
                .collect(),
        }
    }
    fn build(
        node: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        RevisionMapNode::new(
            node.prefix,
            node.value,
            node.children
                .into_iter()
                .map(|(edge, child)| RevisionMapChild { edge, child })
                .collect(),
        )
    }
    fn id(node: &Self::PublicNode) -> Self::Id {
        node.id()
    }
    fn canonical_bytes(node: &Self::PublicNode) -> Vec<u8> {
        node.canonical_bytes()
    }
}

pub struct RevisionMapStore<'a, S> {
    inner: radix::Store<'a, S, RevisionCollection>,
}

/// Ordered, disk-backed traversal retaining at most one node per key byte.
/// The cursor is exhausted after a storage or structural error.
pub struct RevisionMapCursor<'a, S> {
    inner: radix::Cursor<'a, S, RevisionCollection>,
}

impl<S: ImmutableObjectStore> RevisionMapCursor<'_, S> {
    pub async fn next(&mut self) -> Result<Option<(ChangeId, RevisionId)>, RevisionMapError> {
        self.inner.next().await.map_err(Into::into)
    }
}

impl<'a, S: ImmutableObjectStore> RevisionMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
        }
    }
    pub async fn empty_root(&self) -> Result<RevisionMapId, RevisionMapError> {
        Ok(self.inner.empty_root().await?)
    }
    pub async fn read_node(&self, id: RevisionMapId) -> Result<RevisionMapNode, RevisionMapError> {
        Ok(self.inner.read_public(id).await?)
    }
    pub async fn cursor(
        &self,
        root: RevisionMapId,
    ) -> Result<RevisionMapCursor<'a, S>, RevisionMapError> {
        Ok(RevisionMapCursor {
            inner: self.inner.cursor(root).await?,
        })
    }
    pub async fn get(
        &self,
        root: RevisionMapId,
        key: ChangeId,
    ) -> Result<Option<RevisionId>, RevisionMapError> {
        Ok(self.inner.get(root, key).await?)
    }
    pub async fn insert(
        &self,
        root: RevisionMapId,
        key: ChangeId,
        value: RevisionId,
    ) -> Result<RevisionMapId, RevisionMapError> {
        self.update_batch(root, [(key, Some(value))]).await
    }
    pub async fn remove(
        &self,
        root: RevisionMapId,
        key: ChangeId,
    ) -> Result<RevisionMapId, RevisionMapError> {
        self.update_batch(root, [(key, None)]).await
    }
    /// Apply a batch with last-write-wins duplicate keys and one rebuild per shared prefix.
    pub async fn update_batch(
        &self,
        root: RevisionMapId,
        edits: impl IntoIterator<Item = (ChangeId, Option<RevisionId>)>,
    ) -> Result<RevisionMapId, RevisionMapError> {
        Ok(self.inner.update_batch(root, edits).await?)
    }
    pub async fn contains_node(&self, id: RevisionMapId) -> Result<bool, RevisionMapError> {
        Ok(self.inner.contains_node(id).await?)
    }
}
