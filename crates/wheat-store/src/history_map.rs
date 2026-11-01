//! A view's history by time: a radix map from [`HistoryKey`] (when a change's revision
//! was committed, then the change, newest first) to the application that applied it.

use thiserror::Error;
use wheat_core::{
    ApplicationId, HistoryKey, HistoryMapChild, HistoryMapId, HistoryMapNode,
    decode_history_map_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

#[derive(Debug, Error)]
pub enum HistoryMapError {
    #[error("immutable history-map storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("history-map node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("history-map root must have depth zero")]
    InvalidRootDepth,
    #[error("history-map child depth does not match its parent")]
    InvalidChildDepth,
    #[error("history-map node has an invalid terminal shape")]
    InvalidTerminalNode,
    #[error("history-map repeats its subtrees under more than {limit} other key paths")]
    RepeatedSubtrees { limit: usize },
    #[error("history-map delta contains more than {limit} entries")]
    DeltaChangeLimitExceeded { limit: usize },
    #[error("history-map delta reads more than {limit} nodes")]
    DeltaNodeLimitExceeded { limit: usize },
    #[error("history-map delta queues more than {limit} pending subtrees")]
    DeltaPendingLimitExceeded { limit: usize },
}
impl From<radix::Error> for HistoryMapError {
    fn from(e: radix::Error) -> Self {
        match e {
            radix::Error::Store(e) => Self::Store(e),
            radix::Error::Decode(e) => Self::Decode(e),
            radix::Error::InvalidRootDepth => Self::InvalidRootDepth,
            radix::Error::InvalidChildDepth => Self::InvalidChildDepth,
            radix::Error::InvalidTerminalNode => Self::InvalidTerminalNode,
            radix::Error::RepeatedSubtrees { limit } => Self::RepeatedSubtrees { limit },
        }
    }
}
impl From<radix::DiffError> for HistoryMapError {
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

/// One deterministic difference from an old history map to a new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryMapDelta {
    Added {
        key: HistoryKey,
        application: ApplicationId,
    },
    Changed {
        key: HistoryKey,
        old_application: ApplicationId,
        new_application: ApplicationId,
    },
    Removed {
        key: HistoryKey,
        application: ApplicationId,
    },
}

impl radix::Key for HistoryKey {
    const LEN: usize = HistoryKey::LEN;
    fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }
    fn from_bytes(bytes: &[u8]) -> Self {
        Self::from_bytes(bytes.try_into().expect("radix key length is fixed"))
    }
}

pub(crate) struct HistoryCollection;
impl radix::Collection for HistoryCollection {
    type Id = HistoryMapId;
    type Key = HistoryKey;
    type Value = ApplicationId;
    type PublicNode = HistoryMapNode;
    const KIND: ObjectKind = ObjectKind::HistoryMapNode;
    fn decode(b: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_history_map_node(b)
    }
    fn unpack(n: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
        radix::Node {
            prefix: n.prefix,
            value: n.value,
            children: n.children.into_iter().map(|c| (c.edge, c.child)).collect(),
            entries: n.entries,
        }
    }
    fn build(
        n: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        HistoryMapNode::new(
            n.prefix,
            n.value,
            n.children
                .into_iter()
                .map(|(edge, child)| HistoryMapChild { edge, child })
                .collect(),
            n.entries,
        )
    }
    fn id(n: &Self::PublicNode) -> Self::Id {
        n.id()
    }
    fn canonical_bytes(n: &Self::PublicNode) -> Vec<u8> {
        n.canonical_bytes()
    }
}

pub struct HistoryMapStore<'a, S> {
    inner: radix::Store<'a, S, HistoryCollection>,
}
/// Reads a history map in key order: newest first.
pub struct HistoryMapCursor<'a, S> {
    inner: radix::Cursor<'a, S, HistoryCollection>,
}
impl<S: ImmutableObjectStore> HistoryMapCursor<'_, S> {
    pub async fn next(&mut self) -> Result<Option<(HistoryKey, ApplicationId)>, HistoryMapError> {
        Ok(self.inner.next().await?)
    }
}
impl<'a, S: ImmutableObjectStore> HistoryMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
        }
    }
    pub async fn empty_root(&self) -> Result<HistoryMapId, HistoryMapError> {
        Ok(self.inner.empty_root().await?)
    }
    pub async fn read_node(&self, id: HistoryMapId) -> Result<HistoryMapNode, HistoryMapError> {
        Ok(self.inner.read_public(id).await?)
    }
    pub async fn cursor(
        &self,
        root: HistoryMapId,
    ) -> Result<HistoryMapCursor<'a, S>, HistoryMapError> {
        Ok(HistoryMapCursor {
            inner: self.inner.cursor(root).await?,
        })
    }
    /// A cursor at the first key at or after `key`: O(log n) node reads.
    pub async fn cursor_from(
        &self,
        root: HistoryMapId,
        key: HistoryKey,
    ) -> Result<HistoryMapCursor<'a, S>, HistoryMapError> {
        Ok(HistoryMapCursor {
            inner: self.inner.cursor_from(root, &key).await?,
        })
    }
    pub async fn get(
        &self,
        root: HistoryMapId,
        key: HistoryKey,
    ) -> Result<Option<ApplicationId>, HistoryMapError> {
        Ok(self.inner.get(root, key).await?)
    }
    pub async fn insert(
        &self,
        root: HistoryMapId,
        key: HistoryKey,
        value: ApplicationId,
    ) -> Result<HistoryMapId, HistoryMapError> {
        Ok(self.inner.update_batch(root, [(key, Some(value))]).await?)
    }
    pub async fn contains_node(&self, id: HistoryMapId) -> Result<bool, HistoryMapError> {
        Ok(self.inner.contains_node(id).await?)
    }

    /// The entries that differ from `old` to `new`, in key order, stopping with an
    /// error past `max_changes` of them.
    pub async fn delta(
        &self,
        old: HistoryMapId,
        new: HistoryMapId,
        max_changes: usize,
    ) -> Result<Vec<HistoryMapDelta>, HistoryMapError> {
        let entries = self
            .inner
            .paired_diff(
                old,
                new,
                radix::DiffLimits {
                    max_changes,
                    ..radix::DiffLimits::default()
                },
            )
            .await?;
        Ok(entries
            .into_iter()
            .map(|entry| match entry {
                radix::DiffEntry::Added(key, application) => {
                    HistoryMapDelta::Added { key, application }
                }
                radix::DiffEntry::Changed(key, old_application, new_application) => {
                    HistoryMapDelta::Changed {
                        key,
                        old_application,
                        new_application,
                    }
                }
                radix::DiffEntry::Removed(key, application) => {
                    HistoryMapDelta::Removed { key, application }
                }
            })
            .collect())
    }
}
