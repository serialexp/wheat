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
    #[error("application-map node has an invalid terminal shape")]
    InvalidTerminalNode,
    #[error("application-map delta contains more than {limit} entries")]
    DeltaChangeLimitExceeded { limit: usize },
    #[error("application-map delta reads more than {limit} nodes")]
    DeltaNodeLimitExceeded { limit: usize },
    #[error("application-map delta queues more than {limit} pending subtrees")]
    DeltaPendingLimitExceeded { limit: usize },
}
impl From<radix::Error> for ApplicationMapError {
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
impl From<radix::DiffError> for ApplicationMapError {
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

/// One deterministic difference from an old application map to a new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationMapDelta {
    Added {
        change: ChangeId,
        application: ApplicationId,
    },
    Changed {
        change: ChangeId,
        old_application: ApplicationId,
        new_application: ApplicationId,
    },
    Removed {
        change: ChangeId,
        application: ApplicationId,
    },
}

/// Bounds for a read-only application-map comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationMapDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for ApplicationMapDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
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
            prefix: n.prefix,
            value: n.value,
            children: n.children.into_iter().map(|c| (c.edge, c.child)).collect(),
        }
    }
    fn build(
        n: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        ApplicationMapNode::new(
            n.prefix,
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

    /// Return lexicographically ordered entry changes from `old` to `new`.
    pub async fn delta(
        &self,
        old: ApplicationMapId,
        new: ApplicationMapId,
        limits: ApplicationMapDeltaLimits,
    ) -> Result<Vec<ApplicationMapDelta>, ApplicationMapError> {
        let entries = self
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
            .await?;
        Ok(entries
            .into_iter()
            .map(|entry| match entry {
                radix::DiffEntry::Added(change, application) => ApplicationMapDelta::Added {
                    change,
                    application,
                },
                radix::DiffEntry::Changed(change, old_application, new_application) => {
                    ApplicationMapDelta::Changed {
                        change,
                        old_application,
                        new_application,
                    }
                }
                radix::DiffEntry::Removed(change, application) => ApplicationMapDelta::Removed {
                    change,
                    application,
                },
            })
            .collect())
    }
}
