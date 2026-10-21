use thiserror::Error;
use wheat_core::{
    ApplicationId, ConflictId, ConflictSetChild, ConflictSetId, ConflictSetNode, ContentId,
    ContentSetChild, ContentSetId, ContentSetNode, ObjectId, ProjectedCommit,
    ProjectedCommitSetChild, ProjectedCommitSetId, ProjectedCommitSetNode, RetainedSetChild,
    RetainedSetId, RetainedSetNode, decode_conflict_set_node, decode_content_set_node,
    decode_projected_commit_set_node, decode_retained_set_node,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError, radix};

/// One authenticated membership difference between two retained-object sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedSetDelta {
    Added(ObjectId),
    Removed(ObjectId),
}

/// One authenticated membership difference between two content sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSetDelta {
    Added(ContentId),
    Removed(ContentId),
}

/// Bounds for a read-only content-set comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentSetDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for ContentSetDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
        }
    }
}

/// Bounds for a read-only retained-set comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedSetDeltaLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_pending: usize,
}

impl Default for RetainedSetDeltaLimits {
    fn default() -> Self {
        let limits = radix::DiffLimits::default();
        Self {
            max_changes: limits.max_changes,
            max_nodes: limits.max_nodes,
            max_pending: limits.max_pending,
        }
    }
}

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
    #[error("set delta contains more than {limit} entries")]
    DeltaChangeLimitExceeded { limit: usize },
    #[error("set delta reads more than {limit} nodes")]
    DeltaNodeLimitExceeded { limit: usize },
    #[error("set delta queues more than {limit} pending subtrees")]
    DeltaPendingLimitExceeded { limit: usize },
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
impl From<radix::DiffError> for CollectionError {
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

impl radix::Key for ProjectedCommit {
    const LEN: usize = ProjectedCommit::LEN;
    fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }
    fn from_bytes(bytes: &[u8]) -> Self {
        Self::from_bytes(bytes.try_into().expect("radix key length is fixed"))
    }
}

macro_rules! set_collection {
    ($collection:ident,$id:ty,$key:ty,$node:ty,$child:ident,$kind:ident,$decode:ident) => {
        pub(crate) struct $collection;
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
                    entries: n.entries.into_iter().map(|suffix| (suffix, ())).collect(),
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
                    n.entries.into_iter().map(|(suffix, ())| suffix).collect(),
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
set_collection!(
    ProjectedCommitCollection,
    ProjectedCommitSetId,
    ProjectedCommit,
    ProjectedCommitSetNode,
    ProjectedCommitSetChild,
    ProjectedCommitSetNode,
    decode_projected_commit_set_node
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
            #[allow(dead_code)] // Only retained sets currently need byte-accounted lineage reads.
            pub(crate) async fn read_node_at_with_size(
                &self,
                id: $id,
                position: usize,
            ) -> Result<($node, usize), CollectionError> {
                Ok(self.inner.read_public_at_with_size(id, position).await?)
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
            /// Equal subtrees are skipped by identity and only the keys `other` adds are
            /// written, so time and memory follow how much the roots differ.
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

impl<S: ImmutableObjectStore> ConflictSetStore<'_, S> {
    /// The conflicts `new` adds to `old` and removes from it, stopping at more than
    /// `max_changes` differences. Equal subtrees are pruned by identity.
    pub(crate) async fn delta(
        &self,
        old: ConflictSetId,
        new: ConflictSetId,
        max_changes: usize,
    ) -> Result<Vec<radix::DiffEntry<ConflictId, ()>>, CollectionError> {
        Ok(self
            .inner
            .paired_diff(
                old,
                new,
                radix::DiffLimits {
                    max_changes,
                    ..radix::DiffLimits::default()
                },
            )
            .await?)
    }
}

set_api!(
    ContentSetStore,
    ContentSetCursor,
    ContentCollection,
    ContentSetId,
    ContentId,
    ContentSetNode
);

impl<S: ImmutableObjectStore> ContentSetStore<'_, S> {
    /// Compare two content sets without writing derived collection nodes.
    ///
    /// Equal immutable subtrees are pruned by identity; divergent nodes are read
    /// and structurally validated at their contextual radix positions.
    pub async fn delta(
        &self,
        old: ContentSetId,
        new: ContentSetId,
        limits: ContentSetDeltaLimits,
    ) -> Result<Vec<ContentSetDelta>, CollectionError> {
        Ok(self
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
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(id, ()) => Some(ContentSetDelta::Added(id)),
                radix::DiffEntry::Removed(id, ()) => Some(ContentSetDelta::Removed(id)),
                radix::DiffEntry::Changed(_, (), ()) => None,
            })
            .collect())
    }

    /// The members of `new` that `old` lacks, in order. Unlike [`Self::delta`] there
    /// is no change limit: callers want every added blob (publish and fetch want
    /// lists), and shared subtrees are still skipped by identity.
    pub async fn added(
        &self,
        old: ContentSetId,
        new: ContentSetId,
    ) -> Result<Vec<ContentId>, CollectionError> {
        Ok(self
            .inner
            .paired_diff(old, new, radix::DiffLimits::unbounded())
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(id, ()) => Some(id),
                radix::DiffEntry::Removed(..) | radix::DiffEntry::Changed(..) => None,
            })
            .collect())
    }
}

set_api!(
    RetainedSetStore,
    RetainedSetCursor,
    RetainedCollection,
    RetainedSetId,
    ObjectId,
    RetainedSetNode
);

set_api!(
    ProjectedCommitSetStore,
    ProjectedCommitSetCursor,
    ProjectedCommitCollection,
    ProjectedCommitSetId,
    ProjectedCommit,
    ProjectedCommitSetNode
);

/// One authenticated membership difference between two projected-commit sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectedCommitSetDelta {
    Added(ProjectedCommit),
    Removed(ProjectedCommit),
}

impl<S: ImmutableObjectStore> ProjectedCommitSetStore<'_, S> {
    /// The applications that project as `commit`, in order. Members lead with the
    /// commit, so only the nodes on the path to it and its own members are read.
    pub async fn applications(
        &self,
        root: ProjectedCommitSetId,
        commit: [u8; ProjectedCommit::COMMIT_LEN],
    ) -> Result<Vec<ApplicationId>, CollectionError> {
        let lowest = ProjectedCommit::new(commit, ApplicationId::from_digest([0; 32]));
        let mut cursor = self.inner.cursor_from(root, &lowest).await?;
        let mut applications = Vec::new();
        while let Some((member, ())) = cursor.next().await? {
            if member.commit() != commit {
                break;
            }
            applications.push(member.application());
        }
        Ok(applications)
    }

    /// Compare two sets without writing derived collection nodes. Equal subtrees are
    /// pruned by identity.
    pub async fn delta(
        &self,
        old: ProjectedCommitSetId,
        new: ProjectedCommitSetId,
        limits: RetainedSetDeltaLimits,
    ) -> Result<Vec<ProjectedCommitSetDelta>, CollectionError> {
        Ok(self
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
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(member, ()) => Some(ProjectedCommitSetDelta::Added(member)),
                radix::DiffEntry::Removed(member, ()) => {
                    Some(ProjectedCommitSetDelta::Removed(member))
                }
                radix::DiffEntry::Changed(_, (), ()) => None,
            })
            .collect())
    }
}

impl<S: ImmutableObjectStore> RetainedSetStore<'_, S> {
    /// Compare two retained-object sets without writing derived collection nodes.
    ///
    /// Equal immutable subtrees are pruned by identity; divergent nodes are read
    /// and structurally validated at their contextual radix positions.
    pub async fn delta(
        &self,
        old: RetainedSetId,
        new: RetainedSetId,
        limits: RetainedSetDeltaLimits,
    ) -> Result<Vec<RetainedSetDelta>, CollectionError> {
        Ok(self
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
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                radix::DiffEntry::Added(id, ()) => Some(RetainedSetDelta::Added(id)),
                radix::DiffEntry::Removed(id, ()) => Some(RetainedSetDelta::Removed(id)),
                radix::DiffEntry::Changed(_, (), ()) => None,
            })
            .collect())
    }
}
