use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationMapChild, ApplicationMapId, ApplicationMapNode, ChangeId,
    decode_application_map_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

#[derive(Debug, Error)]
pub enum ApplicationMapError {
    #[error("immutable application-map storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("application-map node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("application-map root must have depth zero")]
    InvalidRootDepth,
    #[error("application-map child depth does not match its parent")]
    InvalidChildDepth,
}
impl From<radix::Error> for ApplicationMapError {
    fn from(e: radix::Error) -> Self {
        match e {
            radix::Error::Store(e) => Self::Store(e),
            radix::Error::Decode(e) => Self::Decode(e),
            radix::Error::InvalidRootDepth => Self::InvalidRootDepth,
            radix::Error::InvalidChildDepth => Self::InvalidChildDepth,
        }
    }
}

pub(crate) struct ApplicationCollection;
impl radix::Collection for ApplicationCollection {
    type Id = ApplicationMapId;
    type Key = ChangeId;
    type Value = ApplicationId;
    type PublicNode = ApplicationMapNode;
    const KIND: ObjectKind = ObjectKind::ApplicationMapNode;
    fn decode(b: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_application_map_node(b)
    }
    fn unpack(n: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
        radix::Node {
            depth: n.depth,
            value: n.value,
            children: n.children.into_iter().map(|c| (c.edge, c.child)).collect(),
        }
    }
    fn build(
        n: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        ApplicationMapNode::new(
            n.depth,
            n.value,
            n.children
                .into_iter()
                .map(|(edge, child)| ApplicationMapChild { edge, child })
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

pub struct ApplicationMapStore<'a, S> {
    inner: radix::Store<'a, S, ApplicationCollection>,
}
pub struct ApplicationMapCursor<'a, S> {
    inner: radix::Cursor<'a, S, ApplicationCollection>,
}
impl<S: ImmutableObjectStore> ApplicationMapCursor<'_, S> {
    pub async fn next(&mut self) -> Result<Option<(ChangeId, ApplicationId)>, ApplicationMapError> {
        Ok(self.inner.next().await?)
    }
}
impl<'a, S: ImmutableObjectStore> ApplicationMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
        }
    }
    pub async fn empty_root(&self) -> Result<ApplicationMapId, ApplicationMapError> {
        Ok(self.inner.empty_root().await?)
    }
    pub async fn read_node(
        &self,
        id: ApplicationMapId,
    ) -> Result<ApplicationMapNode, ApplicationMapError> {
        Ok(self.inner.read_public(id).await?)
    }
    pub async fn cursor(
        &self,
        root: ApplicationMapId,
    ) -> Result<ApplicationMapCursor<'a, S>, ApplicationMapError> {
        Ok(ApplicationMapCursor {
            inner: self.inner.cursor(root).await?,
        })
    }
    pub async fn get(
        &self,
        root: ApplicationMapId,
        key: ChangeId,
    ) -> Result<Option<ApplicationId>, ApplicationMapError> {
        Ok(self.inner.get(root, key).await?)
    }
    pub async fn insert(
        &self,
        root: ApplicationMapId,
        key: ChangeId,
        value: ApplicationId,
    ) -> Result<ApplicationMapId, ApplicationMapError> {
        self.update_batch(root, [(key, Some(value))]).await
    }
    pub async fn remove(
        &self,
        root: ApplicationMapId,
        key: ChangeId,
    ) -> Result<ApplicationMapId, ApplicationMapError> {
        self.update_batch(root, [(key, None)]).await
    }
    pub async fn update_batch(
        &self,
        root: ApplicationMapId,
        edits: impl IntoIterator<Item = (ChangeId, Option<ApplicationId>)>,
    ) -> Result<ApplicationMapId, ApplicationMapError> {
        Ok(self.inner.update_batch(root, edits).await?)
    }
    pub async fn contains_node(&self, id: ApplicationMapId) -> Result<bool, ApplicationMapError> {
        Ok(self.inner.contains_node(id).await?)
    }
}
