use std::collections::BTreeMap;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEdit {
    pub path: Vec<String>,
    pub value: Option<FileValue>,
}

pub struct MerkleTreeStore<'a, S> {
    objects: &'a S,
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

    pub async fn contains_node(&self, id: TreeId) -> Result<bool, MerkleTreeError> {
        Ok(self.objects.exists_many(&[ObjectId::from(id)]).await?[0])
    }
}
