use std::collections::BTreeMap;

use thiserror::Error;
use wheat_core::{
    ContentId, ContentMapChild, ContentMapId, ContentMapNode, decode_content_map_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

#[derive(Debug, Error)]
pub enum ContentMapError {
    #[error("immutable content-map storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("content-map node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("content-map root must have depth zero")]
    InvalidRootDepth,
    #[error("content-map child depth does not match its parent")]
    InvalidChildDepth,
    #[error("content-map node has an invalid terminal shape")]
    InvalidTerminalNode,
    #[error("content-map delta contains more than {limit} entries")]
    DeltaChangeLimitExceeded { limit: usize },
    #[error("content-map delta reads more than {limit} nodes")]
    DeltaNodeLimitExceeded { limit: usize },
    #[error("content-map delta queues more than {limit} pending subtrees")]
    DeltaPendingLimitExceeded { limit: usize },
    #[error("content {content} would be named by fewer than zero tree entries")]
    CountUnderflow { content: ContentId },
    #[error("content {content} would be named by more than u32::MAX tree entries")]
    CountOverflow { content: ContentId },
}
impl From<radix::Error> for ContentMapError {
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
impl From<radix::DiffError> for ContentMapError {
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

/// One difference from an old content map to a new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentMapDelta {
    Added {
        content: ContentId,
        count: u32,
    },
    Changed {
        content: ContentId,
        old: u32,
        new: u32,
    },
    Removed {
        content: ContentId,
        count: u32,
    },
}

/// Bounds for a read-only content-map comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentMapDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for ContentMapDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
        }
    }
}

pub(crate) struct ContentMapCollection;
impl radix::Collection for ContentMapCollection {
    type Id = ContentMapId;
    type Key = ContentId;
    type Value = u32;
    type PublicNode = ContentMapNode;
    const KIND: ObjectKind = ObjectKind::ContentMapNode;
    fn decode(b: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_content_map_node(b)
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
        ContentMapNode::new(
            n.prefix,
            n.value,
            n.children
                .into_iter()
                .map(|(edge, child)| ContentMapChild { edge, child })
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

/// A view's content map: each blob its tree names, with the number of tree entries
/// naming it.
pub struct ContentMapStore<'a, S> {
    inner: radix::Store<'a, S, ContentMapCollection>,
}
pub struct ContentMapCursor<'a, S> {
    inner: radix::Cursor<'a, S, ContentMapCollection>,
}
impl<S: ImmutableObjectStore> ContentMapCursor<'_, S> {
    pub async fn next(&mut self) -> Result<Option<(ContentId, u32)>, ContentMapError> {
        Ok(self.inner.next().await?)
    }
}
impl<'a, S: ImmutableObjectStore> ContentMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
        }
    }
    pub async fn empty_root(&self) -> Result<ContentMapId, ContentMapError> {
        Ok(self.inner.empty_root().await?)
    }
    pub async fn read_node(&self, id: ContentMapId) -> Result<ContentMapNode, ContentMapError> {
        Ok(self.inner.read_public(id).await?)
    }
    /// Read and contextually validate a node reached at the given key position.
    pub async fn read_node_at(
        &self,
        id: ContentMapId,
        position: usize,
    ) -> Result<ContentMapNode, ContentMapError> {
        Ok(self.inner.read_public_at(id, position).await?)
    }
    pub async fn cursor(
        &self,
        root: ContentMapId,
    ) -> Result<ContentMapCursor<'a, S>, ContentMapError> {
        Ok(ContentMapCursor {
            inner: self.inner.cursor(root).await?,
        })
    }
    pub async fn get(
        &self,
        root: ContentMapId,
        content: ContentId,
    ) -> Result<Option<u32>, ContentMapError> {
        Ok(self.inner.get(root, content).await?)
    }
    /// Sets each blob's count; `None` (or a count of 0) removes it.
    pub async fn update_batch(
        &self,
        root: ContentMapId,
        edits: impl IntoIterator<Item = (ContentId, Option<u32>)>,
    ) -> Result<ContentMapId, ContentMapError> {
        Ok(self
            .inner
            .update_batch(
                root,
                edits
                    .into_iter()
                    .map(|(content, count)| (content, count.filter(|count| *count > 0))),
            )
            .await?)
    }
    /// Adds each signed change to its blob's count, reading only the blobs named:
    /// a tree diff's +1 for each entry gained and -1 for each entry lost. A count
    /// that reaches 0 removes the blob.
    pub async fn adjust(
        &self,
        root: ContentMapId,
        changes: impl IntoIterator<Item = (ContentId, i64)>,
    ) -> Result<ContentMapId, ContentMapError> {
        let mut net = BTreeMap::<ContentId, i64>::new();
        for (content, change) in changes {
            *net.entry(content).or_default() += change;
        }
        let mut edits = Vec::with_capacity(net.len());
        for (content, change) in net {
            if change == 0 {
                continue;
            }
            let current = i64::from(self.get(root, content).await?.unwrap_or(0));
            let count = current + change;
            if count < 0 {
                return Err(ContentMapError::CountUnderflow { content });
            }
            let count =
                u32::try_from(count).map_err(|_| ContentMapError::CountOverflow { content })?;
            edits.push((content, Some(count)));
        }
        self.update_batch(root, edits).await
    }
    /// The blobs `new` names that `old` does not, in order. Like
    /// [`crate::ContentSetStore::added`] there is no change limit, and shared subtrees
    /// are skipped by identity, so the cost follows how much the maps differ.
    pub async fn added(
        &self,
        old: ContentMapId,
        new: ContentMapId,
    ) -> Result<Vec<ContentId>, ContentMapError> {
        Ok(self
            .inner
            .paired_diff(old, new, radix::DiffLimits::unbounded())
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(content, _) => Some(content),
                radix::DiffEntry::Changed(..) | radix::DiffEntry::Removed(..) => None,
            })
            .collect())
    }
    pub async fn contains_node(&self, id: ContentMapId) -> Result<bool, ContentMapError> {
        Ok(self.inner.contains_node(id).await?)
    }

    /// Return lexicographically ordered entry changes from `old` to `new`.
    pub async fn delta(
        &self,
        old: ContentMapId,
        new: ContentMapId,
        limits: ContentMapDeltaLimits,
    ) -> Result<Vec<ContentMapDelta>, ContentMapError> {
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
                radix::DiffEntry::Added(content, count) => {
                    ContentMapDelta::Added { content, count }
                }
                radix::DiffEntry::Changed(content, old, new) => {
                    ContentMapDelta::Changed { content, old, new }
                }
                radix::DiffEntry::Removed(content, count) => {
                    ContentMapDelta::Removed { content, count }
                }
            })
            .collect())
    }
}
