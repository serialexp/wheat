use std::{
    cmp::Ordering,
    collections::{BTreeMap, btree_map},
    iter::Peekable,
};

use thiserror::Error;
use wheat_core::{
    DirectoryEntry, DirectoryNode, FileValue, ObjectId, TreeId, decode_directory_node,
    validate_directory_component,
};

use crate::{ImmutableObjectStore, ObjectKind, ObjectStoreError};

#[derive(Debug, Error)]
pub enum MerkleTreeError {
    #[error("immutable tree storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("tree node does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error("tree path must contain at least one component")]
    EmptyPath,
    #[error("tree path crosses file component {0:?}")]
    FileInPath(String),
    #[error("tree contains more than {limit} files")]
    FileLimitExceeded { limit: usize },
    #[error("tree path is {size} bytes, exceeding the {limit}-byte limit")]
    PathBytesExceeded { size: usize, limit: usize },
    #[error("tree depth {depth} exceeds the limit of {limit}")]
    DepthExceeded { depth: usize, limit: usize },
    #[error("tree diff contains more than {limit} changes")]
    ChangeLimitExceeded { limit: usize },
    #[error("tree traversal reads more than {limit} nodes")]
    NodeLimitExceeded { limit: usize },
    #[error("tree traversal visits more than {limit} directory entries")]
    EntryLimitExceeded { limit: usize },
    #[error("tree node is {size} bytes, exceeding the {limit}-byte limit")]
    NodeBytesExceeded { size: usize, limit: usize },
    #[error("tree traversal buffers more than {limit} directory entries")]
    BufferedEntriesExceeded { limit: usize },
    #[error("tree selection contains more than {limit} selectors")]
    SelectorLimitExceeded { limit: usize },
    #[error("tree selection selectors contain {size} path bytes, exceeding the {limit}-byte limit")]
    SelectorPathBytesExceeded { size: usize, limit: usize },
    #[error("tree selection contains more than {limit} path components")]
    SelectorComponentLimitExceeded { limit: usize },
    #[error("tree selection selectors are not sorted and unique")]
    NonCanonicalSelectors,
    #[error("tree selector {0:?} is absent")]
    MissingSelector(String),
    #[error("tree update contains more than {limit} edits")]
    EditLimitExceeded { limit: usize },
    #[error("tree update paths contain more than {limit} components")]
    EditComponentLimitExceeded { limit: usize },
    #[error("tree update paths contain {size} bytes, exceeding the {limit}-byte limit")]
    EditPathBytesExceeded { size: usize, limit: usize },
    #[error("tree update stages {size} node bytes, exceeding the {limit}-byte limit")]
    StagedNodeBytesExceeded { size: usize, limit: usize },
}

/// Resource bounds for a complete Merkle tree walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeWalkLimits {
    /// Maximum number of file leaves returned by the walk.
    pub max_files: usize,
    /// Maximum tree nodes read during the walk.
    pub max_nodes: usize,
    /// Maximum directory entries visited during the walk.
    pub max_entries: usize,
    /// Maximum aggregate bytes retained in queued and returned full paths.
    pub max_total_path_bytes: usize,
    /// Maximum byte length of any complete slash-separated path.
    pub max_path_bytes: usize,
    /// Maximum directory nesting, with the root directory at depth zero.
    pub max_depth: usize,
    /// Maximum canonical bytes in one decoded tree node.
    pub max_node_bytes: usize,
    /// Maximum directory entries held by the lexical queue.
    pub max_buffered_entries: usize,
}

impl Default for TreeWalkLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_nodes: 250_000,
            max_entries: 1_000_000,
            max_total_path_bytes: 64 * 1024 * 1024,
            max_path_bytes: 4_096,
            max_depth: 128,
            max_node_bytes: 16 * 1024 * 1024,
            max_buffered_entries: 1_000_000,
        }
    }
}

/// Resource bounds for resolving and walking one pinned tree path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreePathLimits {
    pub max_files: usize,
    pub max_nodes: usize,
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_node_bytes: usize,
    pub max_buffered_entries: usize,
}

impl Default for TreePathLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_nodes: 250_000,
            max_entries: 1_000_000,
            max_path_bytes: 4_096,
            max_depth: 128,
            max_node_bytes: 16 * 1024 * 1024,
            max_buffered_entries: 1_000_000,
        }
    }
}

/// Aggregate resource bounds for selecting multiple paths from one pinned tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeSelectionLimits {
    pub max_files: usize,
    pub max_nodes: usize,
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_node_bytes: usize,
    pub max_buffered_entries: usize,
    pub max_selectors: usize,
    pub max_selector_path_bytes: usize,
    /// Maximum total slash-separated components across all selectors.
    pub max_selector_components: usize,
}

impl Default for TreeSelectionLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_nodes: 250_000,
            max_entries: 1_000_000,
            max_path_bytes: 64 * 1024 * 1024,
            max_depth: 128,
            max_node_bytes: 16 * 1024 * 1024,
            max_buffered_entries: 1_000_000,
            max_selectors: 250_000,
            max_selector_path_bytes: 64 * 1024 * 1024,
            max_selector_components: 1_000_000,
        }
    }
}

/// One file leaf returned by [`MerkleTreeStore::walk_files`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeFile {
    /// Canonical slash-separated repository-relative path.
    pub path: String,
    pub value: FileValue,
}

/// What one path names in a Merkle tree, from [`MerkleTreeStore::path_shapes`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreePathShape {
    /// No entry exists at the path; any ancestors that exist are directories.
    Absent,
    File(FileValue),
    Directory,
    /// The ancestor component at index `depth` of the path is a file.
    BelowFile {
        depth: usize,
    },
}

/// The kind and selected contents of one path in a pinned Merkle tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreePathSelection {
    Missing,
    File(TreeFile),
    Directory(Vec<TreeFile>),
}

/// The paths at or below any of a set of repository paths, like the paths in
/// `git diff -- PATH…`. An empty set selects every path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreePathFilter {
    /// Sorted and deduplicated canonical paths.
    roots: Vec<String>,
}

impl TreePathFilter {
    /// Selects the paths at or below each of `roots`, which are canonical slash-separated
    /// repository-relative paths. No roots selects everything.
    #[must_use]
    pub fn new(roots: impl IntoIterator<Item = String>) -> Self {
        let mut roots = roots.into_iter().collect::<Vec<_>>();
        roots.sort_unstable();
        roots.dedup();
        Self { roots }
    }

    /// Whether every path is selected.
    #[must_use]
    pub fn is_everything(&self) -> bool {
        self.roots.is_empty()
    }

    /// Whether `path` is one of the roots or lies below one. Costs one search per
    /// component of `path`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        if self.is_everything() {
            return true;
        }
        let contains = |candidate: &str| {
            self.roots
                .binary_search_by(|root| root.as_str().cmp(candidate))
                .is_ok()
        };
        path.match_indices('/')
            .map(|(index, _)| &path[..index])
            .chain(std::iter::once(path))
            .any(contains)
    }

    /// Whether a directory at `path` can hold a selected path: it is selected itself,
    /// or it is an ancestor of a root.
    #[must_use]
    pub fn reaches(&self, path: &str) -> bool {
        if self.matches(path) {
            return true;
        }
        // Roots below `path` sort together, from the first one not less than `path/`.
        let below = self.roots.partition_point(|root| {
            root.bytes().cmp(path.bytes().chain(std::iter::once(b'/'))) == Ordering::Less
        });
        self.roots.get(below).is_some_and(|root| {
            root.len() > path.len() && root.starts_with(path) && root.as_bytes()[path.len()] == b'/'
        })
    }
}

/// One changed file leaf between two Merkle roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeDiff {
    /// Canonical slash-separated repository-relative path.
    pub path: String,
    pub old: Option<FileValue>,
    pub new: Option<FileValue>,
}

/// Resource bounds for a Merkle tree diff cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeDiffLimits {
    pub max_changes: usize,
    pub max_nodes: usize,
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_node_bytes: usize,
    pub max_buffered_entries: usize,
}

impl Default for TreeDiffLimits {
    fn default() -> Self {
        Self {
            max_changes: 250_000,
            max_nodes: 250_000,
            max_entries: 1_000_000,
            max_path_bytes: 4_096,
            max_depth: 128,
            max_node_bytes: 16 * 1024 * 1024,
            max_buffered_entries: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEdit {
    pub path: Vec<String>,
    pub value: Option<FileValue>,
}

/// Resource bounds for one atomic final-tree update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalTreeUpdateLimits {
    pub max_edits: usize,
    pub max_path_components: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_node_bytes: usize,
    pub max_total_node_bytes: usize,
}

impl Default for FinalTreeUpdateLimits {
    fn default() -> Self {
        Self {
            max_edits: 250_000,
            max_path_components: 1_000_000,
            max_path_bytes: 64 * 1024 * 1024,
            max_depth: 128,
            max_nodes: 250_000,
            max_node_bytes: 16 * 1024 * 1024,
            max_total_node_bytes: 256 * 1024 * 1024,
        }
    }
}

#[path = "merkle_tree/final_update.rs"]
mod final_update;

pub struct MerkleTreeStore<'a, S> {
    objects: &'a S,
}

type EntryIter = Peekable<btree_map::IntoIter<String, DirectoryEntry>>;

fn matching_selector_range(
    cursor: &mut usize,
    end: usize,
    mut compare: impl FnMut(usize) -> std::cmp::Ordering,
) -> std::ops::Range<usize> {
    use std::cmp::Ordering;

    while *cursor < end {
        match compare(*cursor) {
            Ordering::Less => *cursor += 1,
            Ordering::Greater => return *cursor..*cursor,
            Ordering::Equal => {
                let first = *cursor;
                *cursor += 1;
                while *cursor < end && compare(*cursor) == Ordering::Equal {
                    *cursor += 1;
                }
                return first..*cursor;
            }
        }
    }
    end..end
}

struct DiffFrame {
    prefix_len: usize,
    depth: usize,
    old: EntryIter,
    new: EntryIter,
}

// Keeping the iterator frame inline avoids one heap allocation per visited directory.
#[allow(clippy::large_enum_variant)]
enum DiffWork {
    Directories {
        old: Option<TreeId>,
        new: Option<TreeId>,
        prefix_len: usize,
        depth: usize,
    },
    Entries(DiffFrame),
    Compare {
        path_len: usize,
        depth: usize,
        old: Option<DirectoryEntry>,
        new: Option<DirectoryEntry>,
    },
    Emit {
        path_len: usize,
        old: Option<FileValue>,
        new: Option<FileValue>,
    },
}

/// Incremental, lexically ordered Merkle diff. An error permanently exhausts it.
/// Bounds for a streaming tree-content traversal without path allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeContentWalkLimits {
    pub max_files: usize,
    pub max_nodes: usize,
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_node_bytes: usize,
    pub max_buffered_entries: usize,
}

impl Default for TreeContentWalkLimits {
    fn default() -> Self {
        Self {
            max_files: 1_000_000,
            max_nodes: 1_000_000,
            max_entries: 4_000_000,
            max_depth: 1_023,
            max_node_bytes: 16 * 1024 * 1024,
            max_buffered_entries: 1_000_000,
        }
    }
}

pub struct TreeContentCursor<'a, S> {
    store: &'a MerkleTreeStore<'a, S>,
    limits: TreeContentWalkLimits,
    stack: Vec<(
        std::collections::btree_map::IntoValues<String, DirectoryEntry>,
        usize,
    )>,
    nodes: usize,
    entries: usize,
    buffered_entries: usize,
    files: usize,
    exhausted: bool,
}

pub struct TreeDiffCursor<'a, S> {
    objects: &'a S,
    limits: TreeDiffLimits,
    work: Vec<DiffWork>,
    path: String,
    nodes: usize,
    entries: usize,
    changes: usize,
    buffered_entries: usize,
    exhausted: bool,
    filter: TreePathFilter,
}

impl<'a, S> MerkleTreeStore<'a, S>
where
    S: ImmutableObjectStore,
{
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self { objects }
    }

    pub async fn empty_root(&self) -> Result<TreeId, MerkleTreeError> {
        self.write_node(&DirectoryNode::default()).await
    }

    pub async fn write_node(&self, node: &DirectoryNode) -> Result<TreeId, MerkleTreeError> {
        let bytes = node.canonical_bytes();
        let id = node.id();
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(id)
    }

    pub async fn read_node(&self, id: TreeId) -> Result<DirectoryNode, MerkleTreeError> {
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        Ok(decode_directory_node(&object.canonical_bytes)?)
    }

    async fn read_path_node(
        &self,
        id: TreeId,
        limits: TreePathLimits,
        nodes: &mut usize,
        entries: &mut usize,
    ) -> Result<DirectoryNode, MerkleTreeError> {
        if *nodes == limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: limits.max_nodes,
            });
        }
        *nodes += 1;
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        let size = object.canonical_bytes.len();
        if size > limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size,
                limit: limits.max_node_bytes,
            });
        }
        let node = decode_directory_node(&object.canonical_bytes)?;
        let total = entries.saturating_add(node.entries.len());
        if total > limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: limits.max_entries,
            });
        }
        *entries = total;
        Ok(node)
    }

    async fn read_selection_node(
        &self,
        id: TreeId,
        limits: TreeSelectionLimits,
        nodes: &mut usize,
        entries: &mut usize,
    ) -> Result<DirectoryNode, MerkleTreeError> {
        if *nodes == limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: limits.max_nodes,
            });
        }
        *nodes += 1;
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        let size = object.canonical_bytes.len();
        if size > limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size,
                limit: limits.max_node_bytes,
            });
        }
        let node = decode_directory_node(&object.canonical_bytes)?;
        let total = entries.saturating_add(node.entries.len());
        if total > limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: limits.max_entries,
            });
        }
        *entries = total;
        Ok(node)
    }

    pub async fn lookup(
        &self,
        root: TreeId,
        path: &[String],
    ) -> Result<Option<FileValue>, MerkleTreeError> {
        if path.is_empty() {
            return Err(MerkleTreeError::EmptyPath);
        }
        let mut node = self.read_node(root).await?;
        for (index, component) in path.iter().enumerate() {
            validate_directory_component(component)?;
            let Some(entry) = node.entries.get(component) else {
                return Ok(None);
            };
            let last = index + 1 == path.len();
            match (entry, last) {
                (DirectoryEntry::File(content), true) => return Ok(Some(*content)),
                (DirectoryEntry::Directory(_), true) => return Ok(None),
                (DirectoryEntry::Directory(child), false) => node = self.read_node(*child).await?,
                (DirectoryEntry::File(_), false) => {
                    return Err(MerkleTreeError::FileInPath(component.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Looks up many file paths with one shared traversal (see [`Self::path_shapes`]).
    ///
    /// Results follow `paths`; like [`Self::lookup`], a missing path or a directory yields
    /// `None`, and a file in place of an ancestor directory is [`MerkleTreeError::FileInPath`].
    #[tracing::instrument(name = "tree_lookup_many", level = "debug", skip_all, fields(paths = paths.len()))]
    pub async fn lookup_many(
        &self,
        root: TreeId,
        paths: &[&str],
    ) -> Result<Vec<Option<FileValue>>, MerkleTreeError> {
        let shapes = self.path_shapes(root, paths).await?;
        paths
            .iter()
            .zip(shapes)
            .map(|(path, shape)| match shape {
                TreePathShape::File(value) => Ok(Some(value)),
                TreePathShape::Absent | TreePathShape::Directory => Ok(None),
                TreePathShape::BelowFile { depth } => Err(MerkleTreeError::FileInPath(
                    path.split('/').nth(depth).unwrap_or_default().to_owned(),
                )),
            })
            .collect()
    }

    /// Classifies many paths with one shared traversal.
    ///
    /// Results follow `paths`. The directories along the previous path stay decoded and are
    /// reused by the next one, so lexically sorted `paths` read each directory node at most
    /// once instead of once per path below it. Memory holds one decoded node per open
    /// directory level.
    pub async fn path_shapes(
        &self,
        root: TreeId,
        paths: &[&str],
    ) -> Result<Vec<TreePathShape>, MerkleTreeError> {
        // No path needs the root, so an empty request reads nothing.
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let root_node = self.read_node(root).await?;
        let mut open: Vec<(&str, DirectoryNode)> = Vec::new();
        let mut components: Vec<&str> = Vec::new();
        let mut results = Vec::with_capacity(paths.len());
        for path in paths {
            if path.is_empty() {
                return Err(MerkleTreeError::EmptyPath);
            }
            components.clear();
            for component in path.split('/') {
                validate_directory_component(component)?;
                components.push(component);
            }
            let (leaf, directories) = components.split_last().expect("split yields a component");
            let shared = open
                .iter()
                .zip(directories)
                .take_while(|((open, _), wanted)| open == *wanted)
                .count();
            open.truncate(shared);
            let mut blocked = None;
            for (depth, directory) in directories.iter().enumerate().skip(shared) {
                let node = open.last().map_or(&root_node, |(_, node)| node);
                let child = match node.entries.get(*directory) {
                    None => {
                        blocked = Some(TreePathShape::Absent);
                        break;
                    }
                    Some(DirectoryEntry::File(_)) => {
                        blocked = Some(TreePathShape::BelowFile { depth });
                        break;
                    }
                    Some(DirectoryEntry::Directory(child)) => *child,
                };
                open.push((directory, self.read_node(child).await?));
            }
            let shape = blocked.unwrap_or_else(|| {
                let node = open.last().map_or(&root_node, |(_, node)| node);
                match node.entries.get(*leaf) {
                    Some(DirectoryEntry::File(value)) => TreePathShape::File(*value),
                    Some(DirectoryEntry::Directory(_)) => TreePathShape::Directory,
                    None => TreePathShape::Absent,
                }
            });
            results.push(shape);
        }
        Ok(results)
    }

    /// Resolve one path and, for a directory, return only its descendant files.
    ///
    /// Empty `path` selects the root directory. Output paths are canonical full
    /// paths and lexical. The iterative walk retains one decoded node per open
    /// directory and allocates per leaf only for the returned path.
    pub async fn walk_path(
        &self,
        root: TreeId,
        path: &[String],
        limits: TreePathLimits,
    ) -> Result<TreePathSelection, MerkleTreeError> {
        let mut full_path = String::new();
        for component in path {
            validate_directory_component(component)?;
            let size = full_path
                .len()
                .checked_add(usize::from(!full_path.is_empty()))
                .and_then(|size| size.checked_add(component.len()))
                .unwrap_or(usize::MAX);
            if size > limits.max_path_bytes {
                return Err(MerkleTreeError::PathBytesExceeded {
                    size,
                    limit: limits.max_path_bytes,
                });
            }
            full_path.reserve(size.saturating_sub(full_path.len()));
            if !full_path.is_empty() {
                full_path.push('/');
            }
            full_path.push_str(component);
        }

        let mut nodes = 0usize;
        let mut entries = 0usize;
        let mut depth = 0usize;
        let mut node = self
            .read_path_node(root, limits, &mut nodes, &mut entries)
            .await?;
        if node.entries.len() > limits.max_buffered_entries {
            return Err(MerkleTreeError::BufferedEntriesExceeded {
                limit: limits.max_buffered_entries,
            });
        }

        for (index, component) in path.iter().enumerate() {
            let Some(entry) = node.entries.remove(component) else {
                return Ok(TreePathSelection::Missing);
            };
            let last = index + 1 == path.len();
            match entry {
                DirectoryEntry::File(value) if last => {
                    return Ok(TreePathSelection::File(TreeFile {
                        path: full_path,
                        value,
                    }));
                }
                DirectoryEntry::File(_) => {
                    return Err(MerkleTreeError::FileInPath(component.clone()));
                }
                DirectoryEntry::Directory(child) => {
                    depth = depth.saturating_add(1);
                    if depth > limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth,
                            limit: limits.max_depth,
                        });
                    }
                    node = self
                        .read_path_node(child, limits, &mut nodes, &mut entries)
                        .await?;
                    if node.entries.len() > limits.max_buffered_entries {
                        return Err(MerkleTreeError::BufferedEntriesExceeded {
                            limit: limits.max_buffered_entries,
                        });
                    }
                    if last {
                        break;
                    }
                }
            }
        }

        struct WalkFrame {
            prefix_len: usize,
            depth: usize,
            entries: btree_map::IntoIter<String, DirectoryEntry>,
        }

        let mut buffered_entries = node.entries.len();
        let mut stack = Vec::with_capacity(limits.max_depth.min(128).saturating_add(1));
        stack.push(WalkFrame {
            prefix_len: full_path.len(),
            depth,
            entries: node.entries.into_iter(),
        });
        let mut files = Vec::new();
        while let Some(frame) = stack.last_mut() {
            let Some((name, entry)) = frame.entries.next() else {
                stack.pop();
                continue;
            };
            buffered_entries -= 1;
            full_path.truncate(frame.prefix_len);
            let size = full_path
                .len()
                .checked_add(usize::from(!full_path.is_empty()))
                .and_then(|size| size.checked_add(name.len()))
                .unwrap_or(usize::MAX);
            if size > limits.max_path_bytes {
                return Err(MerkleTreeError::PathBytesExceeded {
                    size,
                    limit: limits.max_path_bytes,
                });
            }
            full_path.reserve(size.saturating_sub(full_path.len()));
            if !full_path.is_empty() {
                full_path.push('/');
            }
            full_path.push_str(&name);
            match entry {
                DirectoryEntry::File(value) => {
                    if files.len() == limits.max_files {
                        return Err(MerkleTreeError::FileLimitExceeded {
                            limit: limits.max_files,
                        });
                    }
                    files.push(TreeFile {
                        path: full_path.clone(),
                        value,
                    });
                }
                DirectoryEntry::Directory(child) => {
                    let child_depth = frame.depth.saturating_add(1);
                    if child_depth > limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth: child_depth,
                            limit: limits.max_depth,
                        });
                    }
                    let child_node = self
                        .read_path_node(child, limits, &mut nodes, &mut entries)
                        .await?;
                    let buffered = buffered_entries.saturating_add(child_node.entries.len());
                    if buffered > limits.max_buffered_entries {
                        return Err(MerkleTreeError::BufferedEntriesExceeded {
                            limit: limits.max_buffered_entries,
                        });
                    }
                    buffered_entries = buffered;
                    stack.push(WalkFrame {
                        prefix_len: size,
                        depth: child_depth,
                        entries: child_node.entries.into_iter(),
                    });
                }
            }
        }
        Ok(TreePathSelection::Directory(files))
    }

    /// Select multiple files or directory subtrees with one shared, bounded traversal.
    ///
    /// `selectors` must be sorted and unique canonical slash-separated paths. Only
    /// selector ancestors and selected directory subtrees are read. Returned leaves
    /// are deduplicated and lexically ordered, including for overlapping selectors.
    pub async fn select_paths(
        &self,
        root: TreeId,
        selectors: &[String],
        limits: TreeSelectionLimits,
    ) -> Result<Vec<TreeFile>, MerkleTreeError> {
        if selectors.is_empty() || selectors.len() > limits.max_selectors {
            return Err(MerkleTreeError::SelectorLimitExceeded {
                limit: limits.max_selectors,
            });
        }

        struct Selector {
            component_start: usize,
            component_len: usize,
        }
        #[derive(Clone, Copy)]
        struct Component {
            start: usize,
            end: usize,
        }
        struct SelectionFrame {
            prefix_len: usize,
            depth: usize,
            selector_cursor: usize,
            selector_end: usize,
            component_depth: usize,
            selected: bool,
            entries: btree_map::IntoIter<String, DirectoryEntry>,
        }

        let mut selector_path_bytes = 0usize;
        let mut compact = Vec::new();
        let mut parsed = Vec::with_capacity(selectors.len());
        let mut previous: Option<&str> = None;
        for selector in selectors {
            if selector.is_empty() || previous.is_some_and(|prior| prior >= selector.as_str()) {
                return Err(MerkleTreeError::NonCanonicalSelectors);
            }
            selector_path_bytes = selector_path_bytes.saturating_add(selector.len());
            if selector_path_bytes > limits.max_selector_path_bytes {
                return Err(MerkleTreeError::SelectorPathBytesExceeded {
                    size: selector_path_bytes,
                    limit: limits.max_selector_path_bytes,
                });
            }
            let component_start = compact.len();
            let mut offset = 0usize;
            for component in selector.split('/') {
                validate_directory_component(component)?;
                if compact.len() == limits.max_selector_components {
                    return Err(MerkleTreeError::SelectorComponentLimitExceeded {
                        limit: limits.max_selector_components,
                    });
                }
                let end = offset + component.len();
                compact.push(Component { start: offset, end });
                offset = end.saturating_add(1);
            }
            let component_len = compact.len() - component_start;
            if component_len > limits.max_depth.saturating_add(1) {
                return Err(MerkleTreeError::DepthExceeded {
                    depth: component_len - 1,
                    limit: limits.max_depth,
                });
            }
            parsed.push(Selector {
                component_start,
                component_len,
            });
            previous = Some(selector);
        }
        let component = |selector_index: usize, depth: usize| {
            let item = &parsed[selector_index];
            let span = compact[item.component_start + depth];
            &selectors[selector_index][span.start..span.end]
        };

        let mut nodes = 0usize;
        let mut entries = 0usize;
        let root_node = self
            .read_selection_node(root, limits, &mut nodes, &mut entries)
            .await?;
        let mut buffered_entries = root_node.entries.len();
        if buffered_entries > limits.max_buffered_entries {
            return Err(MerkleTreeError::BufferedEntriesExceeded {
                limit: limits.max_buffered_entries,
            });
        }
        let mut stack = Vec::with_capacity(limits.max_depth.min(128).saturating_add(1));
        stack.push(SelectionFrame {
            prefix_len: 0,
            depth: 0,
            selector_cursor: 0,
            selector_end: selectors.len(),
            component_depth: 0,
            selected: false,
            entries: root_node.entries.into_iter(),
        });
        let mut matched = vec![false; selectors.len()];
        let mut files = Vec::new();
        let mut path = String::new();
        let mut output_path_bytes = 0usize;

        while let Some(frame) = stack.last_mut() {
            let Some((name, entry)) = frame.entries.next() else {
                stack.pop();
                continue;
            };
            buffered_entries -= 1;
            let frame_component_depth = frame.component_depth;
            let frame_selected = frame.selected;
            let range = matching_selector_range(
                &mut frame.selector_cursor,
                frame.selector_end,
                |selector_index| {
                    if parsed[selector_index].component_len <= frame_component_depth {
                        std::cmp::Ordering::Less
                    } else {
                        component(selector_index, frame_component_depth).cmp(name.as_str())
                    }
                },
            );
            let first = range.start;
            let end = range.end;
            if !frame_selected && first == end {
                continue;
            }

            path.truncate(frame.prefix_len);
            let path_len = path
                .len()
                .checked_add(usize::from(!path.is_empty()))
                .and_then(|size| size.checked_add(name.len()))
                .unwrap_or(usize::MAX);
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&name);

            match entry {
                DirectoryEntry::File(value) => {
                    let mut terminal = false;
                    for selector_index in first..end {
                        if parsed[selector_index].component_len == frame_component_depth + 1 {
                            matched[selector_index] = true;
                            terminal = true;
                        }
                    }
                    if frame_selected || terminal {
                        if files.len() == limits.max_files {
                            return Err(MerkleTreeError::FileLimitExceeded {
                                limit: limits.max_files,
                            });
                        }
                        output_path_bytes = output_path_bytes.saturating_add(path_len);
                        if output_path_bytes > limits.max_path_bytes {
                            return Err(MerkleTreeError::PathBytesExceeded {
                                size: output_path_bytes,
                                limit: limits.max_path_bytes,
                            });
                        }
                        files.push(TreeFile {
                            path: path.clone(),
                            value,
                        });
                    }
                }
                DirectoryEntry::Directory(child) => {
                    let child_depth = frame.depth.saturating_add(1);
                    if child_depth > limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth: child_depth,
                            limit: limits.max_depth,
                        });
                    }
                    let mut selected = frame_selected;
                    for selector_index in first..end {
                        if parsed[selector_index].component_len == frame_component_depth + 1 {
                            matched[selector_index] = true;
                            selected = true;
                        }
                    }
                    let child_node = self
                        .read_selection_node(child, limits, &mut nodes, &mut entries)
                        .await?;
                    let buffered = buffered_entries.saturating_add(child_node.entries.len());
                    if buffered > limits.max_buffered_entries {
                        return Err(MerkleTreeError::BufferedEntriesExceeded {
                            limit: limits.max_buffered_entries,
                        });
                    }
                    buffered_entries = buffered;
                    stack.push(SelectionFrame {
                        prefix_len: path_len,
                        depth: child_depth,
                        selector_cursor: first,
                        selector_end: end,
                        component_depth: frame_component_depth + 1,
                        selected,
                        entries: child_node.entries.into_iter(),
                    });
                }
            }
        }

        if let Some(index) = matched.iter().position(|matched| !matched) {
            return Err(MerkleTreeError::MissingSelector(selectors[index].clone()));
        }
        Ok(files)
    }

    /// Return every file below `root` in lexical full-path order.
    ///
    /// Traversal is iterative, and each bound is checked before the entry that
    /// would exceed it is accepted. Directory storage and decoding failures are
    /// returned unchanged.
    pub async fn walk_files(
        &self,
        root: TreeId,
        limits: TreeWalkLimits,
    ) -> Result<Vec<TreeFile>, MerkleTreeError> {
        let mut nodes = 0usize;
        let mut entries = 0usize;
        let mut retained_path_bytes = 0usize;
        let root_node = self
            .read_walk_node(root, limits, &mut nodes, &mut entries)
            .await?;
        let mut files = Vec::new();
        let mut pending = BTreeMap::new();
        for (path, entry) in root_node.entries {
            retained_path_bytes = retained_path_bytes.checked_add(path.len()).ok_or(
                MerkleTreeError::PathBytesExceeded {
                    size: usize::MAX,
                    limit: limits.max_total_path_bytes,
                },
            )?;
            pending.insert(path, (entry, 0usize));
        }
        if pending.len() > limits.max_buffered_entries {
            return Err(MerkleTreeError::BufferedEntriesExceeded {
                limit: limits.max_buffered_entries,
            });
        }
        if retained_path_bytes > limits.max_total_path_bytes {
            return Err(MerkleTreeError::PathBytesExceeded {
                size: retained_path_bytes,
                limit: limits.max_total_path_bytes,
            });
        }

        while let Some((path, (entry, depth))) = pending.pop_first() {
            retained_path_bytes -= path.len();
            if path.len() > limits.max_path_bytes {
                return Err(MerkleTreeError::PathBytesExceeded {
                    size: path.len(),
                    limit: limits.max_path_bytes,
                });
            }

            match entry {
                DirectoryEntry::File(value) => {
                    if files.len() == limits.max_files {
                        return Err(MerkleTreeError::FileLimitExceeded {
                            limit: limits.max_files,
                        });
                    }
                    retained_path_bytes = retained_path_bytes.checked_add(path.len()).ok_or(
                        MerkleTreeError::PathBytesExceeded {
                            size: usize::MAX,
                            limit: limits.max_total_path_bytes,
                        },
                    )?;
                    if retained_path_bytes > limits.max_total_path_bytes {
                        return Err(MerkleTreeError::PathBytesExceeded {
                            size: retained_path_bytes,
                            limit: limits.max_total_path_bytes,
                        });
                    }
                    files.push(TreeFile { path, value });
                }
                DirectoryEntry::Directory(child) => {
                    let child_depth =
                        depth.checked_add(1).ok_or(MerkleTreeError::DepthExceeded {
                            depth: usize::MAX,
                            limit: limits.max_depth,
                        })?;
                    if child_depth > limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth: child_depth,
                            limit: limits.max_depth,
                        });
                    }
                    let child_node = self
                        .read_walk_node(child, limits, &mut nodes, &mut entries)
                        .await?;
                    let buffered = pending.len().saturating_add(child_node.entries.len());
                    if buffered > limits.max_buffered_entries {
                        return Err(MerkleTreeError::BufferedEntriesExceeded {
                            limit: limits.max_buffered_entries,
                        });
                    }
                    for (name, child_entry) in child_node.entries {
                        let mut child_path = String::with_capacity(path.len() + 1 + name.len());
                        child_path.push_str(&path);
                        child_path.push('/');
                        child_path.push_str(&name);
                        retained_path_bytes = retained_path_bytes
                            .checked_add(child_path.len())
                            .ok_or(MerkleTreeError::PathBytesExceeded {
                                size: usize::MAX,
                                limit: limits.max_total_path_bytes,
                            })?;
                        if retained_path_bytes > limits.max_total_path_bytes {
                            return Err(MerkleTreeError::PathBytesExceeded {
                                size: retained_path_bytes,
                                limit: limits.max_total_path_bytes,
                            });
                        }
                        pending.insert(child_path, (child_entry, child_depth));
                    }
                }
            }
        }

        Ok(files)
    }

    async fn read_walk_node(
        &self,
        id: TreeId,
        limits: TreeWalkLimits,
        nodes: &mut usize,
        entries: &mut usize,
    ) -> Result<DirectoryNode, MerkleTreeError> {
        if *nodes == limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: limits.max_nodes,
            });
        }
        *nodes += 1;
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        if object.canonical_bytes.len() > limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size: object.canonical_bytes.len(),
                limit: limits.max_node_bytes,
            });
        }
        let node = decode_directory_node(&object.canonical_bytes)?;
        *entries = entries.saturating_add(node.entries.len());
        if *entries > limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: limits.max_entries,
            });
        }
        Ok(node)
    }

    #[tracing::instrument(name = "tree_apply_edits", level = "debug", skip_all, fields(edits = edits.len()))]
    pub async fn apply_edits(
        &self,
        root: TreeId,
        edits: &[TreeEdit],
    ) -> Result<TreeId, MerkleTreeError> {
        if edits.is_empty() {
            return Ok(root);
        }
        let root_node = self.read_node(root).await?;
        let root_node = self.edit_node(root_node, edits).await?;
        self.write_node(&root_node).await
    }

    /// Applies `edits` to `node` and returns the result unwritten. Every changed
    /// directory below it is written, except those left empty, which are dropped.
    async fn edit_node(
        &self,
        mut node: DirectoryNode,
        edits: &[TreeEdit],
    ) -> Result<DirectoryNode, MerkleTreeError> {
        let mut grouped = BTreeMap::<String, Vec<TreeEdit>>::new();
        for edit in edits {
            let Some((first, rest)) = edit.path.split_first() else {
                return Err(MerkleTreeError::EmptyPath);
            };
            validate_directory_component(first)?;
            grouped.entry(first.clone()).or_default().push(TreeEdit {
                path: rest.to_vec(),
                value: edit.value,
            });
        }
        for (name, child_edits) in grouped {
            let direct = child_edits.iter().find(|edit| edit.path.is_empty());
            if let Some(edit) = direct {
                if child_edits.len() != 1 {
                    return Err(MerkleTreeError::FileInPath(name));
                }
                match edit.value {
                    Some(value) => {
                        node.entries.insert(name, DirectoryEntry::File(value));
                    }
                    None => {
                        node.entries.remove(&name);
                    }
                }
                continue;
            }

            let child = match node.entries.get(&name) {
                Some(DirectoryEntry::Directory(id)) => self.read_node(*id).await?,
                Some(DirectoryEntry::File(_)) => return Err(MerkleTreeError::FileInPath(name)),
                None => DirectoryNode::default(),
            };
            let child = Box::pin(self.edit_node(child, &child_edits)).await?;
            if child.entries.is_empty() {
                node.entries.remove(&name);
            } else {
                let child = self.write_node(&child).await?;
                node.entries.insert(name, DirectoryEntry::Directory(child));
            }
        }
        Ok(node)
    }

    /// Apply mixed deletions and insertions as one final-tree update.
    ///
    /// Unlike [`Self::apply_edits`], this accepts the paired edits needed for a
    /// file/directory shape transition. It persists only final nodes, never a
    /// deletion-only intermediate tree.
    pub async fn apply_final_edits(
        &self,
        root: TreeId,
        edits: &[TreeEdit],
    ) -> Result<TreeId, MerkleTreeError> {
        self.apply_final_edits_with_limits(root, edits, FinalTreeUpdateLimits::default())
            .await
    }

    pub async fn apply_final_edits_with_limits(
        &self,
        root: TreeId,
        edits: &[TreeEdit],
        limits: FinalTreeUpdateLimits,
    ) -> Result<TreeId, MerkleTreeError> {
        final_update::apply(self, root, edits, limits).await
    }

    /// Stream file values without allocating or retaining full path strings.
    pub async fn content_cursor(
        &'a self,
        root: TreeId,
        limits: TreeContentWalkLimits,
    ) -> Result<TreeContentCursor<'a, S>, MerkleTreeError> {
        let mut cursor = TreeContentCursor {
            store: self,
            limits,
            stack: Vec::with_capacity(32),
            nodes: 0,
            entries: 0,
            buffered_entries: 0,
            files: 0,
            exhausted: false,
        };
        let node = cursor.read_node(root).await?;
        cursor.stack.push((node.entries.into_values(), 0));
        Ok(cursor)
    }

    /// Create an incremental lexical diff from `old` to `new`.
    ///
    /// Equal roots are pruned without reading storage. Other equal directory IDs
    /// are pruned as soon as their parent entries are merged.
    #[must_use]
    pub fn diff_cursor(
        &self,
        old: TreeId,
        new: TreeId,
        limits: TreeDiffLimits,
    ) -> TreeDiffCursor<'a, S> {
        let mut work = Vec::with_capacity(limits.max_depth.min(128).saturating_add(1));
        if old != new {
            work.push(DiffWork::Directories {
                old: Some(old),
                new: Some(new),
                prefix_len: 0,
                depth: 0,
            });
        }
        TreeDiffCursor {
            objects: self.objects,
            limits,
            work,
            path: String::new(),
            nodes: 0,
            entries: 0,
            changes: 0,
            buffered_entries: 0,
            exhausted: false,
            filter: TreePathFilter::default(),
        }
    }

    pub async fn contains_node(&self, id: TreeId) -> Result<bool, MerkleTreeError> {
        Ok(self.objects.exists_many(&[ObjectId::from(id)]).await?[0])
    }
}

impl<S: ImmutableObjectStore> TreeContentCursor<'_, S> {
    async fn read_node(&mut self, id: TreeId) -> Result<DirectoryNode, MerkleTreeError> {
        if self.nodes == self.limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            });
        }
        self.nodes += 1;
        let object = self.store.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        if object.canonical_bytes.len() > self.limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size: object.canonical_bytes.len(),
                limit: self.limits.max_node_bytes,
            });
        }
        let node = decode_directory_node(&object.canonical_bytes)?;
        let entries = self.entries.saturating_add(node.entries.len());
        if entries > self.limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: self.limits.max_entries,
            });
        }
        let buffered = self.buffered_entries.saturating_add(node.entries.len());
        if buffered > self.limits.max_buffered_entries {
            return Err(MerkleTreeError::BufferedEntriesExceeded {
                limit: self.limits.max_buffered_entries,
            });
        }
        self.entries = entries;
        self.buffered_entries = buffered;
        Ok(node)
    }

    /// Return the next file value. Errors permanently exhaust the cursor.
    pub async fn next(&mut self) -> Result<Option<FileValue>, MerkleTreeError> {
        if self.exhausted {
            return Ok(None);
        }
        let result = self.next_inner().await;
        if result.is_err() || matches!(result, Ok(None)) {
            self.exhausted = true;
            self.stack.clear();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<FileValue>, MerkleTreeError> {
        loop {
            let Some((entries, depth)) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(entry) = entries.next() else {
                self.stack.pop();
                continue;
            };
            self.buffered_entries -= 1;
            match entry {
                DirectoryEntry::File(value) => {
                    if self.files == self.limits.max_files {
                        return Err(MerkleTreeError::FileLimitExceeded {
                            limit: self.limits.max_files,
                        });
                    }
                    self.files += 1;
                    return Ok(Some(value));
                }
                DirectoryEntry::Directory(child) => {
                    let child_depth =
                        depth.checked_add(1).ok_or(MerkleTreeError::DepthExceeded {
                            depth: usize::MAX,
                            limit: self.limits.max_depth,
                        })?;
                    if child_depth > self.limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth: child_depth,
                            limit: self.limits.max_depth,
                        });
                    }
                    let node = self.read_node(child).await?;
                    self.stack.push((node.entries.into_values(), child_depth));
                }
            }
        }
    }
}

impl<S: ImmutableObjectStore> TreeDiffCursor<'_, S> {
    /// Reports only changes to paths `filter` matches. Directories that cannot hold a
    /// selected path are not read, and the change limit counts only reported changes.
    #[must_use]
    pub fn restricted_to(mut self, filter: TreePathFilter) -> Self {
        self.filter = filter;
        self
    }

    async fn read_node(&mut self, id: TreeId) -> Result<DirectoryNode, MerkleTreeError> {
        if self.nodes == self.limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            });
        }
        self.nodes += 1;
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::Tree {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual: object.kind,
            }
            .into());
        }
        let size = object.canonical_bytes.len();
        if size > self.limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size,
                limit: self.limits.max_node_bytes,
            });
        }
        Ok(decode_directory_node(&object.canonical_bytes)?)
    }

    fn account_node(&mut self, node: &DirectoryNode) -> Result<(), MerkleTreeError> {
        let count = node.entries.len();
        let entries = self.entries.saturating_add(count);
        if entries > self.limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: self.limits.max_entries,
            });
        }
        let buffered = self.buffered_entries.saturating_add(count);
        if buffered > self.limits.max_buffered_entries {
            return Err(MerkleTreeError::BufferedEntriesExceeded {
                limit: self.limits.max_buffered_entries,
            });
        }
        self.entries = entries;
        self.buffered_entries = buffered;
        Ok(())
    }

    fn push_path(&mut self, prefix_len: usize, name: &str) -> Result<usize, MerkleTreeError> {
        self.path.truncate(prefix_len);
        let size = prefix_len
            .checked_add(usize::from(prefix_len != 0))
            .and_then(|size| size.checked_add(name.len()))
            .unwrap_or(usize::MAX);
        if size > self.limits.max_path_bytes {
            return Err(MerkleTreeError::PathBytesExceeded {
                size,
                limit: self.limits.max_path_bytes,
            });
        }
        self.path.reserve(size.saturating_sub(self.path.len()));
        if prefix_len != 0 {
            self.path.push('/');
        }
        self.path.push_str(name);
        Ok(size)
    }

    /// Return the next changed file in lexical path order.
    pub async fn next(&mut self) -> Result<Option<TreeDiff>, MerkleTreeError> {
        if self.exhausted {
            return Ok(None);
        }
        let result = self.next_inner().await;
        if result.is_err() || matches!(result, Ok(None)) {
            self.exhausted = true;
            self.work.clear();
            self.buffered_entries = 0;
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<TreeDiff>, MerkleTreeError> {
        while let Some(work) = self.work.pop() {
            match work {
                DiffWork::Emit { path_len, old, new } => {
                    self.path.truncate(path_len);
                    if !self.filter.matches(&self.path) {
                        continue;
                    }
                    if self.changes == self.limits.max_changes {
                        return Err(MerkleTreeError::ChangeLimitExceeded {
                            limit: self.limits.max_changes,
                        });
                    }
                    self.changes += 1;
                    return Ok(Some(TreeDiff {
                        path: self.path.clone(),
                        old,
                        new,
                    }));
                }
                DiffWork::Directories {
                    old,
                    new,
                    prefix_len,
                    depth,
                } => {
                    if old.is_some() && old == new {
                        continue;
                    }
                    if depth > self.limits.max_depth {
                        return Err(MerkleTreeError::DepthExceeded {
                            depth,
                            limit: self.limits.max_depth,
                        });
                    }
                    let old_node = match old {
                        Some(id) => Some(self.read_node(id).await?),
                        None => None,
                    };
                    let new_node = match new {
                        Some(id) => Some(self.read_node(id).await?),
                        None => None,
                    };
                    if let Some(node) = &old_node {
                        self.account_node(node)?;
                    }
                    if let Some(node) = &new_node {
                        self.account_node(node)?;
                    }
                    self.work.push(DiffWork::Entries(DiffFrame {
                        prefix_len,
                        depth,
                        old: old_node.unwrap_or_default().entries.into_iter().peekable(),
                        new: new_node.unwrap_or_default().entries.into_iter().peekable(),
                    }));
                }
                DiffWork::Entries(mut frame) => {
                    let old_name = frame.old.peek().map(|(name, _)| name.as_str());
                    let new_name = frame.new.peek().map(|(name, _)| name.as_str());
                    if old_name.is_none() && new_name.is_none() {
                        continue;
                    }
                    let take_old =
                        new_name.is_none() || old_name.is_some_and(|old| Some(old) <= new_name);
                    let take_new =
                        old_name.is_none() || new_name.is_some_and(|new| Some(new) <= old_name);
                    if !take_old && !take_new {
                        continue;
                    }
                    let old = take_old.then(|| frame.old.next().expect("peeked old entry"));
                    let new = take_new.then(|| frame.new.next().expect("peeked new entry"));
                    self.buffered_entries -=
                        usize::from(old.is_some()) + usize::from(new.is_some());
                    let name = old.as_ref().map_or_else(
                        || &new.as_ref().expect("one entry exists").0,
                        |entry| &entry.0,
                    );
                    let path_len = self.push_path(frame.prefix_len, name)?;
                    let old = old.map(|(_, entry)| entry);
                    let new = new.map(|(_, entry)| entry);
                    let depth = frame.depth;
                    self.work.push(DiffWork::Entries(frame));
                    self.work.push(DiffWork::Compare {
                        path_len,
                        depth,
                        old,
                        new,
                    });
                }
                DiffWork::Compare {
                    path_len,
                    depth,
                    old,
                    new,
                } => {
                    self.path.truncate(path_len);
                    // A file here is reported only if selected (checked on emit); a
                    // directory is entered only if it can hold a selected path.
                    if !self.filter.reaches(&self.path) {
                        continue;
                    }
                    match (old, new) {
                        (Some(DirectoryEntry::File(old)), Some(DirectoryEntry::File(new))) => {
                            if old != new {
                                self.work.push(DiffWork::Emit {
                                    path_len,
                                    old: Some(old),
                                    new: Some(new),
                                });
                            }
                        }
                        (
                            Some(DirectoryEntry::Directory(old)),
                            Some(DirectoryEntry::Directory(new)),
                        ) => {
                            if old != new {
                                self.work.push(DiffWork::Directories {
                                    old: Some(old),
                                    new: Some(new),
                                    prefix_len: path_len,
                                    depth: depth.saturating_add(1),
                                });
                            }
                        }
                        (Some(DirectoryEntry::File(old)), Some(DirectoryEntry::Directory(new))) => {
                            self.work.push(DiffWork::Directories {
                                old: None,
                                new: Some(new),
                                prefix_len: path_len,
                                depth: depth.saturating_add(1),
                            });
                            self.work.push(DiffWork::Emit {
                                path_len,
                                old: Some(old),
                                new: None,
                            });
                        }
                        (Some(DirectoryEntry::Directory(old)), Some(DirectoryEntry::File(new))) => {
                            self.work.push(DiffWork::Directories {
                                old: Some(old),
                                new: None,
                                prefix_len: path_len,
                                depth: depth.saturating_add(1),
                            });
                            self.work.push(DiffWork::Emit {
                                path_len,
                                old: None,
                                new: Some(new),
                            });
                        }
                        (Some(DirectoryEntry::File(old)), None) => self.work.push(DiffWork::Emit {
                            path_len,
                            old: Some(old),
                            new: None,
                        }),
                        (None, Some(DirectoryEntry::File(new))) => self.work.push(DiffWork::Emit {
                            path_len,
                            old: None,
                            new: Some(new),
                        }),
                        (Some(DirectoryEntry::Directory(old)), None) => {
                            self.work.push(DiffWork::Directories {
                                old: Some(old),
                                new: None,
                                prefix_len: path_len,
                                depth: depth.saturating_add(1),
                            });
                        }
                        (None, Some(DirectoryEntry::Directory(new))) => {
                            self.work.push(DiffWork::Directories {
                                old: None,
                                new: Some(new),
                                prefix_len: path_len,
                                depth: depth.saturating_add(1),
                            });
                        }
                        (None, None) => unreachable!("merged entry has at least one side"),
                    }
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod selector_merge_tests {
    use super::matching_selector_range;
    use std::cell::Cell;

    #[test]
    fn flat_selector_merge_comparisons_are_linear() {
        const ENTRY_COUNT: usize = 20_000;
        const SELECTOR_COUNT: usize = ENTRY_COUNT / 2;

        let comparisons = Cell::new(0usize);
        let mut cursor = 0usize;
        let mut matches = 0usize;
        for entry in 0..ENTRY_COUNT {
            let range = matching_selector_range(&mut cursor, SELECTOR_COUNT, |selector| {
                comparisons.set(comparisons.get() + 1);
                (selector * 2).cmp(&entry)
            });
            if !range.is_empty() {
                matches += 1;
            }
        }

        assert_eq!(matches, SELECTOR_COUNT);
        assert_eq!(cursor, SELECTOR_COUNT);
        // Every comparison either consumes a selector or rejects one entry. Equal
        // selectors cost one additional look-ahead, so the merge remains linear.
        assert!(
            comparisons.get() <= ENTRY_COUNT + SELECTOR_COUNT * 2,
            "{} comparisons exceeded the linear merge bound",
            comparisons.get()
        );
    }
}
