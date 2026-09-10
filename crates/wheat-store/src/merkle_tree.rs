use std::{
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
    #[error("tree diff reads more than {limit} nodes")]
    NodeLimitExceeded { limit: usize },
    #[error("tree diff visits more than {limit} directory entries")]
    EntryLimitExceeded { limit: usize },
    #[error("tree node is {size} bytes, exceeding the {limit}-byte limit")]
    NodeBytesExceeded { size: usize, limit: usize },
    #[error("tree diff buffers more than {limit} directory entries")]
    BufferedEntriesExceeded { limit: usize },
}

/// Resource bounds for a complete Merkle tree walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeWalkLimits {
    /// Maximum number of file leaves returned by the walk.
    pub max_files: usize,
    /// Maximum byte length of any complete slash-separated path.
    pub max_path_bytes: usize,
    /// Maximum directory nesting, with the root directory at depth zero.
    pub max_depth: usize,
}

impl Default for TreeWalkLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_path_bytes: 4_096,
            max_depth: 128,
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

pub struct MerkleTreeStore<'a, S> {
    objects: &'a S,
}

type EntryIter = Peekable<btree_map::IntoIter<String, DirectoryEntry>>;

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
        let root_node = self.read_node(root).await?;
        let mut files = Vec::new();
        let mut pending = root_node
            .entries
            .into_iter()
            .map(|(path, entry)| (path, (entry, 0usize)))
            .collect::<BTreeMap<_, _>>();

        while let Some((path, (entry, depth))) = pending.pop_first() {
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
                    let child_node = self.read_node(child).await?;
                    for (name, child_entry) in child_node.entries {
                        let mut child_path = String::with_capacity(path.len() + 1 + name.len());
                        child_path.push_str(&path);
                        child_path.push('/');
                        child_path.push_str(&name);
                        pending.insert(child_path, (child_entry, child_depth));
                    }
                }
            }
        }

        Ok(files)
    }

    pub async fn apply_edits(
        &self,
        root: TreeId,
        edits: &[TreeEdit],
    ) -> Result<TreeId, MerkleTreeError> {
        if edits.is_empty() {
            return Ok(root);
        }
        let mut root_node = self.read_node(root).await?;
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
                        root_node.entries.insert(name, DirectoryEntry::File(value));
                    }
                    None => {
                        root_node.entries.remove(&name);
                    }
                }
                continue;
            }

            let child = match root_node.entries.get(&name) {
                Some(DirectoryEntry::Directory(id)) => *id,
                Some(DirectoryEntry::File(_)) => return Err(MerkleTreeError::FileInPath(name)),
                None => self.empty_root().await?,
            };
            let child = Box::pin(self.apply_edits(child, &child_edits)).await?;
            let child_node = self.read_node(child).await?;
            if child_node.entries.is_empty() {
                root_node.entries.remove(&name);
            } else {
                root_node
                    .entries
                    .insert(name, DirectoryEntry::Directory(child));
            }
        }
        self.write_node(&root_node).await
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
        }
    }

    pub async fn contains_node(&self, id: TreeId) -> Result<bool, MerkleTreeError> {
        Ok(self.objects.exists_many(&[ObjectId::from(id)]).await?[0])
    }
}

impl<S: ImmutableObjectStore> TreeDiffCursor<'_, S> {
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
                    if self.changes == self.limits.max_changes {
                        return Err(MerkleTreeError::ChangeLimitExceeded {
                            limit: self.limits.max_changes,
                        });
                    }
                    self.path.truncate(path_len);
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
