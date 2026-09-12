use thiserror::Error;
use wheat_core::{
    ChangeId, ChangeMapChild, ChangeMapId, ChangeMapNode, ChangeObjectId, decode_change_map_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

#[derive(Debug, Error)]
pub enum ChangeMapError {
    #[error("immutable change-map storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("change-map node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("change-map root must have depth zero")]
    InvalidRootDepth,
    #[error("change-map child depth does not match its parent")]
    InvalidChildDepth,
    #[error("change-map node has an invalid terminal shape")]
    InvalidTerminalNode,
}

impl From<radix::Error> for ChangeMapError {
    fn from(error: radix::Error) -> Self {
        match error {
            radix::Error::Store(error) => Self::Store(error),
            radix::Error::Decode(error) => Self::Decode(error),
            radix::Error::InvalidRootDepth => Self::InvalidRootDepth,
            radix::Error::InvalidChildDepth => Self::InvalidChildDepth,
            radix::Error::InvalidTerminalNode => Self::InvalidTerminalNode,
        }
    }
}

struct ChangeCollection;
impl radix::Collection for ChangeCollection {
    type Id = ChangeMapId;
    type Key = ChangeId;
    type Value = ChangeObjectId;
    type PublicNode = ChangeMapNode;
    const KIND: ObjectKind = ObjectKind::ChangeMapNode;

    fn decode(bytes: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_change_map_node(bytes)
    }
    fn unpack(node: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
        radix::Node {
            prefix: node.prefix,
            value: node.value,
            children: node
                .children
                .into_iter()
                .map(|child| (child.edge, child.child))
                .collect(),
        }
    }
    fn build(
        node: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        ChangeMapNode::new(
            node.prefix,
            node.value,
            node.children
                .into_iter()
                .map(|(edge, child)| ChangeMapChild { edge, child })
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

pub struct ChangeMapStore<'a, S> {
    inner: radix::Store<'a, S, ChangeCollection>,
}

pub struct ChangeMapCursor<'a, S> {
    inner: radix::Cursor<'a, S, ChangeCollection>,
}

impl<S: ImmutableObjectStore> ChangeMapCursor<'_, S> {
    pub async fn next(&mut self) -> Result<Option<(ChangeId, ChangeObjectId)>, ChangeMapError> {
        Ok(self.inner.next().await?)
    }
}

impl<'a, S: ImmutableObjectStore> ChangeMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
        }
    }
    pub async fn empty_root(&self) -> Result<ChangeMapId, ChangeMapError> {
        Ok(self.inner.empty_root().await?)
    }
    pub async fn read_node(&self, id: ChangeMapId) -> Result<ChangeMapNode, ChangeMapError> {
        Ok(self.inner.read_public(id).await?)
    }
    pub async fn cursor(
        &self,
        root: ChangeMapId,
    ) -> Result<ChangeMapCursor<'a, S>, ChangeMapError> {
        Ok(ChangeMapCursor {
            inner: self.inner.cursor(root).await?,
        })
    }
    pub async fn get(
        &self,
        root: ChangeMapId,
        key: ChangeId,
    ) -> Result<Option<ChangeObjectId>, ChangeMapError> {
        Ok(self.inner.get(root, key).await?)
    }
    pub async fn insert(
        &self,
        root: ChangeMapId,
        key: ChangeId,
        value: ChangeObjectId,
    ) -> Result<ChangeMapId, ChangeMapError> {
        Ok(self.inner.update_batch(root, [(key, Some(value))]).await?)
    }
    pub async fn contains_node(&self, id: ChangeMapId) -> Result<bool, ChangeMapError> {
        Ok(self.inner.contains_node(id).await?)
    }
}
