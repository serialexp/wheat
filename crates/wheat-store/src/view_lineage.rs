use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::{
    ObjectId, RetainedSetId, ViewId, ViewLineageMapChild, ViewLineageMapId, ViewLineageMapNode,
    decode_view_lineage_map_node,
};

use crate::{
    CollectionError, ImmutableObjectStore, ObjectKind, ObjectStoreError, RetainedSetStore, radix,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewLineageLimits {
    pub max_structural_nodes: usize,
    pub max_map_nodes: usize,
    pub max_set_nodes: usize,
    pub max_decoded_bytes: usize,
    pub max_frontiers: usize,
    pub max_reachable_views: usize,
    pub max_edges: usize,
    pub max_pending: usize,
}

impl Default for ViewLineageLimits {
    fn default() -> Self {
        Self {
            max_structural_nodes: 500_000,
            max_map_nodes: 250_000,
            max_set_nodes: 500_000,
            max_decoded_bytes: 256 * 1024 * 1024,
            max_frontiers: 250_000,
            max_reachable_views: 500_000,
            max_edges: 1_000_000,
            max_pending: 500_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum ViewLineageError {
    #[error("immutable view-lineage storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("view-lineage object does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("view-lineage child-set operation failed")]
    ChildSet(#[from] CollectionError),
    #[error("view-lineage root must have depth zero")]
    InvalidRootDepth,
    #[error("view-lineage child depth does not match its parent")]
    InvalidChildDepth,
    #[error("view-lineage node has an invalid terminal shape")]
    InvalidTerminalNode,
    #[error("view-lineage structural-node limit exceeded ({limit})")]
    StructuralNodeLimitExceeded { limit: usize },
    #[error("view-lineage map-node limit exceeded ({limit})")]
    MapNodeLimitExceeded { limit: usize },
    #[error("view-lineage child-set-node limit exceeded ({limit})")]
    SetNodeLimitExceeded { limit: usize },
    #[error("view-lineage decoded-byte limit exceeded ({limit})")]
    DecodedByteLimitExceeded { limit: usize },
    #[error("view-lineage frontier limit exceeded ({limit})")]
    FrontierLimitExceeded { limit: usize },
    #[error("view-lineage reachable-view limit exceeded ({limit})")]
    ReachableViewLimitExceeded { limit: usize },
    #[error("view-lineage edge limit exceeded ({limit})")]
    EdgeLimitExceeded { limit: usize },
    #[error("view-lineage parent {0} is not retained")]
    ParentNotRetained(ViewId),
    #[error("view-lineage child {0} is not retained")]
    ChildNotRetained(ViewId),
    #[error("view-lineage contains a self edge at {0}")]
    SelfEdge(ViewId),
    #[error("view-lineage contains a cycle")]
    Cycle,
    #[error("view-lineage pending-work limit exceeded ({limit})")]
    PendingLimitExceeded { limit: usize },
    #[error("new view lineage dropped edge {parent} -> {child}")]
    MissingEdge { parent: ViewId, child: ViewId },
}

impl From<radix::Error> for ViewLineageError {
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

pub(crate) struct ViewLineageCollection;
impl radix::Key for ViewId {
    const LEN: usize = 32;

    fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self::from_digest(bytes.try_into().expect("radix key length is fixed"))
    }
}
impl radix::Collection for ViewLineageCollection {
    type Id = ViewLineageMapId;
    type Key = ViewId;
    type Value = RetainedSetId;
    type PublicNode = ViewLineageMapNode;

    const KIND: ObjectKind = ObjectKind::ViewLineageMapNode;

    fn decode(bytes: &[u8]) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        decode_view_lineage_map_node(bytes)
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
            entries: node.entries,
        }
    }

    fn build(
        node: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, wheat_core::CanonicalObjectError> {
        ViewLineageMapNode::new(
            node.prefix,
            node.value,
            node.children
                .into_iter()
                .map(|(edge, child)| ViewLineageMapChild { edge, child })
                .collect(),
            node.entries,
        )
    }

    fn id(node: &Self::PublicNode) -> Self::Id {
        node.id()
    }

    fn canonical_bytes(node: &Self::PublicNode) -> Vec<u8> {
        node.canonical_bytes()
    }
}

pub struct ViewLineageMapStore<'a, S> {
    inner: radix::Store<'a, S, ViewLineageCollection>,
    retained: RetainedSetStore<'a, S>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ViewLineageValidation {
    pub map_nodes: usize,
    pub set_nodes: usize,
    pub decoded_bytes: usize,
    pub vertices: usize,
    pub edges: usize,
}

#[derive(Default)]
struct Budget {
    structural_nodes: usize,
    map_nodes: usize,
    set_nodes: usize,
    decoded_bytes: usize,
    reachable_views: usize,
    edges: usize,
}

impl Budget {
    fn decoded_map(
        &mut self,
        bytes: usize,
        limits: ViewLineageLimits,
    ) -> Result<(), ViewLineageError> {
        self.map_nodes = self.map_nodes.saturating_add(1);
        if self.map_nodes > limits.max_map_nodes {
            return Err(ViewLineageError::MapNodeLimitExceeded {
                limit: limits.max_map_nodes,
            });
        }
        self.decoded(bytes, limits)
    }

    fn decoded_set(
        &mut self,
        bytes: usize,
        limits: ViewLineageLimits,
    ) -> Result<(), ViewLineageError> {
        self.set_nodes = self.set_nodes.saturating_add(1);
        if self.set_nodes > limits.max_set_nodes {
            return Err(ViewLineageError::SetNodeLimitExceeded {
                limit: limits.max_set_nodes,
            });
        }
        self.decoded(bytes, limits)
    }

    fn decoded(&mut self, bytes: usize, limits: ViewLineageLimits) -> Result<(), ViewLineageError> {
        self.structural_nodes = self.structural_nodes.saturating_add(1);
        if self.structural_nodes > limits.max_structural_nodes {
            return Err(ViewLineageError::StructuralNodeLimitExceeded {
                limit: limits.max_structural_nodes,
            });
        }
        self.decoded_bytes = self.decoded_bytes.saturating_add(bytes);
        if self.decoded_bytes > limits.max_decoded_bytes {
            return Err(ViewLineageError::DecodedByteLimitExceeded {
                limit: limits.max_decoded_bytes,
            });
        }
        Ok(())
    }

    fn reachable(&mut self, limits: ViewLineageLimits) -> Result<(), ViewLineageError> {
        self.reachable_views = self.reachable_views.saturating_add(1);
        if self.reachable_views > limits.max_reachable_views {
            return Err(ViewLineageError::ReachableViewLimitExceeded {
                limit: limits.max_reachable_views,
            });
        }
        Ok(())
    }

    fn edge(&mut self, limits: ViewLineageLimits) -> Result<(), ViewLineageError> {
        self.edges = self.edges.saturating_add(1);
        if self.edges > limits.max_edges {
            return Err(ViewLineageError::EdgeLimitExceeded {
                limit: limits.max_edges,
            });
        }
        Ok(())
    }
}

impl<'a, S: ImmutableObjectStore> ViewLineageMapStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self {
            inner: radix::Store::new(objects),
            retained: RetainedSetStore::new(objects),
        }
    }

    pub async fn empty_root(&self) -> Result<ViewLineageMapId, ViewLineageError> {
        Ok(self.inner.empty_root().await?)
    }

    pub async fn read_node(
        &self,
        id: ViewLineageMapId,
    ) -> Result<ViewLineageMapNode, ViewLineageError> {
        Ok(self.inner.read_public(id).await?)
    }

    pub async fn contains_node(&self, id: ViewLineageMapId) -> Result<bool, ViewLineageError> {
        Ok(self.inner.contains_node(id).await?)
    }

    pub async fn get(
        &self,
        root: ViewLineageMapId,
        view: ViewId,
    ) -> Result<Option<RetainedSetId>, ViewLineageError> {
        Ok(self.inner.get(root, view).await?)
    }

    /// Inserts edges after grouping children by parent, updating each child set once.
    #[tracing::instrument(name = "view_lineage_insert", level = "debug", skip_all)]
    pub async fn insert_edges(
        &self,
        root: ViewLineageMapId,
        edges: impl IntoIterator<Item = (ViewId, ViewId)>,
    ) -> Result<ViewLineageMapId, ViewLineageError> {
        let mut edges: Vec<_> = edges.into_iter().collect();
        if edges.is_empty() {
            return Ok(root);
        }
        edges.sort_unstable();
        edges.dedup();

        let empty_children = self.retained.empty_root().await?;
        let mut edits = Vec::with_capacity(edges.len());
        let mut start = 0;
        while start < edges.len() {
            let parent = edges[start].0;
            let mut end = start + 1;
            while end < edges.len() && edges[end].0 == parent {
                end += 1;
            }
            let child_root = self
                .inner
                .get(root, parent)
                .await?
                .unwrap_or(empty_children);
            let child_root = self
                .retained
                .update_batch(
                    child_root,
                    edges[start..end]
                        .iter()
                        .map(|(_, child)| (ObjectId::from(*child), true)),
                )
                .await?;
            edits.push((parent, Some(child_root)));
            start = end;
        }
        Ok(self.inner.update_batch(root, edits).await?)
    }

    /// Every edge of `left` and of `right`. A parent with children on both sides gets
    /// the union of its two child sets.
    ///
    /// Subtrees the roots share are skipped by identity, so two lineages that grew
    /// apart from a common one cost the edges they differ by, not the whole history.
    #[tracing::instrument(name = "view_lineage_union", level = "debug", skip_all)]
    pub async fn union(
        &self,
        left: ViewLineageMapId,
        right: ViewLineageMapId,
    ) -> Result<ViewLineageMapId, ViewLineageError> {
        if left == right {
            return Ok(left);
        }
        let entries = self
            .inner
            .paired_diff(left, right, radix::DiffLimits::unbounded())
            .await
            .map_err(|error| ViewLineageError::from(error.into_unbounded()))?;
        let mut edits = Vec::new();
        for entry in entries {
            match entry {
                radix::DiffEntry::Added(parent, children) => edits.push((parent, Some(children))),
                radix::DiffEntry::Changed(parent, left_children, right_children) => {
                    let children = self.retained.union(left_children, right_children).await?;
                    edits.push((parent, Some(children)));
                }
                radix::DiffEntry::Removed(..) => {}
            }
        }
        Ok(self.inner.update_batch(left, edits).await?)
    }

    /// The parents whose child sets differ between `old` and `new`, with both sides'
    /// child sets. Subtrees the roots share are skipped by identity, so the cost follows
    /// how many parents changed.
    pub(crate) async fn parent_delta(
        &self,
        old: ViewLineageMapId,
        new: ViewLineageMapId,
    ) -> Result<Vec<radix::DiffEntry<ViewId, RetainedSetId>>, ViewLineageError> {
        self.inner
            .paired_diff(old, new, radix::DiffLimits::unbounded())
            .await
            .map_err(|error| ViewLineageError::from(error.into_unbounded()))
    }

    /// Authenticates the complete map and all referenced child sets.
    pub async fn validate(
        &self,
        root: ViewLineageMapId,
        retained_views: RetainedSetId,
        limits: ViewLineageLimits,
    ) -> Result<ViewLineageValidation, ViewLineageError> {
        let mut budget = Budget::default();
        let retained: BTreeSet<_> = self
            .child_set_bounded(retained_views, limits, &mut budget, false)
            .await?
            .into_iter()
            .collect();
        let adjacency = self.adjacency_bounded(root, limits, &mut budget).await?;
        let mut vertices = BTreeSet::new();
        for (parent, children) in &adjacency {
            if !retained.contains(parent) {
                return Err(ViewLineageError::ParentNotRetained(*parent));
            }
            Self::insert_vertex(&mut vertices, *parent, limits)?;
            for child in children {
                if parent == child {
                    return Err(ViewLineageError::SelfEdge(*parent));
                }
                if !retained.contains(child) {
                    return Err(ViewLineageError::ChildNotRetained(*child));
                }
                Self::insert_vertex(&mut vertices, *child, limits)?;
            }
        }

        let mut indegree = BTreeMap::new();
        for vertex in &vertices {
            indegree.insert(*vertex, 0usize);
        }
        for children in adjacency.values() {
            for child in children {
                *indegree.get_mut(child).expect("child vertex was indexed") += 1;
            }
        }
        let mut ready: BTreeSet<_> = indegree
            .iter()
            .filter_map(|(view, degree)| (*degree == 0).then_some(*view))
            .collect();
        let mut consumed = 0usize;
        while let Some(view) = ready.pop_first() {
            consumed += 1;
            if let Some(children) = adjacency.get(&view) {
                for child in children {
                    let degree = indegree.get_mut(child).expect("child vertex was indexed");
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(*child);
                    }
                }
            }
        }
        if consumed != vertices.len() {
            return Err(ViewLineageError::Cycle);
        }
        Ok(ViewLineageValidation {
            map_nodes: budget.map_nodes,
            set_nodes: budget.set_nodes,
            decoded_bytes: budget.decoded_bytes,
            vertices: vertices.len(),
            edges: budget.edges,
        })
    }

    /// Requires every edge in `old_root` to remain present in `new_root`.
    pub async fn validate_extension(
        &self,
        old_root: ViewLineageMapId,
        new_root: ViewLineageMapId,
        limits: ViewLineageLimits,
    ) -> Result<(), ViewLineageError> {
        if old_root == new_root {
            return Ok(());
        }
        let mut budget = Budget::default();
        let old = self
            .adjacency_bounded(old_root, limits, &mut budget)
            .await?;
        let new = self
            .adjacency_bounded(new_root, limits, &mut budget)
            .await?;
        for (parent, old_children) in old {
            let new_children = new.get(&parent);
            for child in old_children {
                if new_children.is_none_or(|children| !children.contains(&child)) {
                    return Err(ViewLineageError::MissingEdge { parent, child });
                }
            }
        }
        Ok(())
    }

    /// Whether `descendant` is reachable from `ancestor` by a forward search.
    ///
    /// The search visits every view below `ancestor` in the worst case, so its cost grows
    /// with the history under `ancestor`.
    pub async fn is_descendant(
        &self,
        root: ViewLineageMapId,
        ancestor: ViewId,
        descendant: ViewId,
        limits: ViewLineageLimits,
    ) -> Result<bool, ViewLineageError> {
        if ancestor == descendant {
            return Ok(true);
        }
        let mut budget = Budget::default();
        let mut pending = BTreeSet::from([ancestor]);
        let mut visited = BTreeSet::new();
        while let Some(view) = pending.pop_first() {
            if !visited.insert(view) {
                continue;
            }
            budget.reachable(limits)?;
            let children = self.children(root, view, limits, &mut budget).await?;
            for child in children {
                if child == descendant {
                    return Ok(true);
                }
                if !visited.contains(&child) {
                    pending.insert(child);
                }
            }
        }
        Ok(false)
    }

    pub async fn descendant_frontiers(
        &self,
        root: ViewLineageMapId,
        basis: ViewId,
        limits: ViewLineageLimits,
    ) -> Result<Vec<ViewId>, ViewLineageError> {
        let mut budget = Budget::default();
        let mut pending = BTreeSet::from([basis]);
        let mut visited = BTreeSet::new();
        let mut frontiers = Vec::new();
        while let Some(view) = pending.pop_first() {
            if !visited.insert(view) {
                continue;
            }
            budget.reachable(limits)?;
            let children = self.children(root, view, limits, &mut budget).await?;
            if children.is_empty() {
                if view != basis {
                    if frontiers.len() == limits.max_frontiers {
                        return Err(ViewLineageError::FrontierLimitExceeded {
                            limit: limits.max_frontiers,
                        });
                    }
                    frontiers.push(view);
                }
                continue;
            }
            for child in children {
                if !visited.contains(&child) {
                    pending.insert(child);
                }
            }
        }
        frontiers.sort_unstable();
        Ok(frontiers)
    }

    fn insert_vertex(
        vertices: &mut BTreeSet<ViewId>,
        view: ViewId,
        limits: ViewLineageLimits,
    ) -> Result<(), ViewLineageError> {
        if vertices.insert(view) && vertices.len() > limits.max_reachable_views {
            return Err(ViewLineageError::ReachableViewLimitExceeded {
                limit: limits.max_reachable_views,
            });
        }
        Ok(())
    }

    async fn adjacency_bounded(
        &self,
        root: ViewLineageMapId,
        limits: ViewLineageLimits,
        budget: &mut Budget,
    ) -> Result<BTreeMap<ViewId, BTreeSet<ViewId>>, ViewLineageError> {
        let mut result = BTreeMap::new();
        let mut key = [0_u8; 32];
        let mut pending = vec![(root, 0usize, None)];
        if pending.len() > limits.max_pending {
            return Err(ViewLineageError::PendingLimitExceeded {
                limit: limits.max_pending,
            });
        }
        while let Some((id, position, edge)) = pending.pop() {
            if let Some(edge) = edge {
                key[position - 1] = edge;
            }
            let (node, decoded_bytes) = self.inner.read_public_at_with_size(id, position).await?;
            budget.decoded_map(decoded_bytes, limits)?;
            let end = position + node.prefix.len();
            key[position..end].copy_from_slice(&node.prefix);
            if let Some(children_root) = node.value {
                let parent = ViewId::from_digest(key);
                let children = self
                    .child_set_bounded(children_root, limits, budget, true)
                    .await?
                    .into_iter()
                    .collect();
                result.insert(parent, children);
            }
            // Yield inline entries.
            for (suffix, children_root) in &node.entries {
                key[end..end + suffix.len()].copy_from_slice(suffix);
                let parent = ViewId::from_digest(key);
                let children = self
                    .child_set_bounded(*children_root, limits, budget, true)
                    .await?
                    .into_iter()
                    .collect();
                result.insert(parent, children);
            }
            for child in node.children.into_iter().rev() {
                pending.push((child.child, end + 1, Some(child.edge)));
            }
            if pending.len() > limits.max_pending {
                return Err(ViewLineageError::PendingLimitExceeded {
                    limit: limits.max_pending,
                });
            }
        }
        Ok(result)
    }

    async fn children(
        &self,
        root: ViewLineageMapId,
        view: ViewId,
        limits: ViewLineageLimits,
        budget: &mut Budget,
    ) -> Result<Vec<ViewId>, ViewLineageError> {
        let Some(child_root) = self.map_get_bounded(root, view, limits, budget).await? else {
            return Ok(Vec::new());
        };
        self.child_set_bounded(child_root, limits, budget, true)
            .await
    }

    async fn map_get_bounded(
        &self,
        mut node_id: ViewLineageMapId,
        key: ViewId,
        limits: ViewLineageLimits,
        budget: &mut Budget,
    ) -> Result<Option<RetainedSetId>, ViewLineageError> {
        let mut position = 0usize;
        loop {
            let (node, decoded_bytes) = self
                .inner
                .read_public_at_with_size(node_id, position)
                .await?;
            budget.decoded_map(decoded_bytes, limits)?;
            let bytes = key.as_bytes();
            if bytes[position..].get(..node.prefix.len()) != Some(node.prefix.as_slice()) {
                return Ok(None);
            }
            position += node.prefix.len();
            if position == 32 {
                return Ok(node.value);
            }
            // Check inline entries first.
            let suffix = &bytes[position..];
            if let Ok(idx) = node
                .entries
                .binary_search_by(|(s, _)| s.as_slice().cmp(suffix))
            {
                return Ok(Some(node.entries[idx].1));
            }
            let edge = bytes[position];
            let Ok(index) = node
                .children
                .binary_search_by_key(&edge, |child| child.edge)
            else {
                return Ok(None);
            };
            node_id = node.children[index].child;
            position += 1;
        }
    }

    async fn child_set_bounded(
        &self,
        root: RetainedSetId,
        limits: ViewLineageLimits,
        budget: &mut Budget,
        count_edges: bool,
    ) -> Result<Vec<ViewId>, ViewLineageError> {
        let mut result = Vec::new();
        let mut key = [0_u8; 32];
        let mut pending = vec![(root, 0usize, None)];
        if pending.len() > limits.max_pending {
            return Err(ViewLineageError::PendingLimitExceeded {
                limit: limits.max_pending,
            });
        }
        while let Some((id, position, edge)) = pending.pop() {
            if let Some(edge) = edge {
                key[position - 1] = edge;
            }
            let (node, decoded_bytes) = self.retained.read_node_at_with_size(id, position).await?;
            budget.decoded_set(decoded_bytes, limits)?;
            let end = position + node.prefix.len();
            key[position..end].copy_from_slice(&node.prefix);
            if node.present {
                if count_edges {
                    budget.edge(limits)?;
                }
                result.push(ViewId::from_digest(key));
            }
            // Yield inline entries (set entries are just suffixes, no values).
            for suffix in &node.entries {
                key[end..end + suffix.len()].copy_from_slice(suffix);
                if count_edges {
                    budget.edge(limits)?;
                }
                result.push(ViewId::from_digest(key));
            }
            for child in node.children.into_iter().rev() {
                pending.push((child.child, end + 1, Some(child.edge)));
            }
            if pending.len() > limits.max_pending {
                return Err(ViewLineageError::PendingLimitExceeded {
                    limit: limits.max_pending,
                });
            }
        }
        Ok(result)
    }
}
