use std::collections::BTreeMap;

use wheat_core::{DirectoryEntry, DirectoryNode, FileValue, TreeId, validate_directory_component};

use super::{FinalTreeUpdateLimits, MerkleTreeError, MerkleTreeStore, TreeEdit};
use crate::ImmutableObjectStore;

#[derive(Default)]
struct FinalEditNode {
    has_direct: bool,
    direct: Option<FileValue>,
    children: BTreeMap<String, FinalEditNode>,
}

enum UpdatedDirectory {
    Empty,
    Tree(TreeId),
}

struct Budget {
    limits: FinalTreeUpdateLimits,
    nodes: usize,
    bytes: usize,
}

impl Budget {
    fn stage(&mut self, node: DirectoryNode) -> Result<DirectoryNode, MerkleTreeError> {
        let bytes = node.canonical_bytes().len();
        if bytes > self.limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size: bytes,
                limit: self.limits.max_node_bytes,
            });
        }
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            });
        }
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.max_total_node_bytes {
            return Err(MerkleTreeError::StagedNodeBytesExceeded {
                size: self.bytes,
                limit: self.limits.max_total_node_bytes,
            });
        }
        Ok(node)
    }
}

pub(super) async fn apply<S: ImmutableObjectStore>(
    store: &MerkleTreeStore<'_, S>,
    root: TreeId,
    edits: &[TreeEdit],
    limits: FinalTreeUpdateLimits,
) -> Result<TreeId, MerkleTreeError> {
    if edits.is_empty() {
        return Ok(root);
    }
    if edits.len() > limits.max_edits {
        return Err(MerkleTreeError::EditLimitExceeded {
            limit: limits.max_edits,
        });
    }

    let mut components = 0usize;
    let mut path_bytes = 0usize;
    let mut final_edits = FinalEditNode::default();
    for edit in edits {
        let Some((first, rest)) = edit.path.split_first() else {
            return Err(MerkleTreeError::EmptyPath);
        };
        if edit.path.len() > limits.max_depth {
            return Err(MerkleTreeError::DepthExceeded {
                depth: edit.path.len(),
                limit: limits.max_depth,
            });
        }
        components = components.saturating_add(edit.path.len());
        if components > limits.max_path_components {
            return Err(MerkleTreeError::EditComponentLimitExceeded {
                limit: limits.max_path_components,
            });
        }
        path_bytes = path_bytes.saturating_add(edit.path.iter().map(String::len).sum::<usize>());
        if path_bytes > limits.max_path_bytes {
            return Err(MerkleTreeError::EditPathBytesExceeded {
                size: path_bytes,
                limit: limits.max_path_bytes,
            });
        }
        validate_directory_component(first)?;
        let mut node = final_edits.children.entry(first.clone()).or_default();
        for component in rest {
            validate_directory_component(component)?;
            node = node.children.entry(component.clone()).or_default();
        }
        if node.has_direct {
            return Err(MerkleTreeError::FileInPath(edit.path.join("/")));
        }
        node.has_direct = true;
        node.direct = edit.value;
    }

    let root_node = store.read_node(root).await?;
    let mut pending = Vec::new();
    let mut budget = Budget {
        limits,
        nodes: 0,
        bytes: 0,
    };
    let updated =
        update_directory(store, root_node, &final_edits, &mut pending, &mut budget).await?;
    let result = match updated {
        UpdatedDirectory::Tree(id) => id,
        UpdatedDirectory::Empty => {
            let empty = DirectoryNode::default();
            let id = empty.id();
            pending.push(empty);
            id
        }
    };
    // Derive and validate every final node before persisting any of them. A
    // rejected shape transition therefore cannot leave orphan tree nodes.
    for node in pending {
        store.write_node(&node).await?;
    }
    Ok(result)
}

async fn update_directory<S: ImmutableObjectStore>(
    store: &MerkleTreeStore<'_, S>,
    mut directory: DirectoryNode,
    edits: &FinalEditNode,
    pending: &mut Vec<DirectoryNode>,
    budget: &mut Budget,
) -> Result<UpdatedDirectory, MerkleTreeError> {
    for (name, child_edits) in &edits.children {
        let old = directory.entries.remove(name);
        let direct = child_edits.has_direct.then_some(child_edits.direct);
        let replacement = match (old, direct, child_edits.children.is_empty()) {
            (old, Some(Some(value)), true) => {
                if matches!(old, Some(DirectoryEntry::Directory(_))) {
                    return Err(MerkleTreeError::FileInPath(name.clone()));
                }
                Some(DirectoryEntry::File(value))
            }
            (Some(DirectoryEntry::Directory(_)), Some(None), true) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (_, Some(None), true) => None,
            (Some(DirectoryEntry::File(_)), Some(None), false) => {
                let empty = DirectoryNode::default();
                match Box::pin(update_directory(store, empty, child_edits, pending, budget)).await?
                {
                    UpdatedDirectory::Tree(id) => Some(DirectoryEntry::Directory(id)),
                    UpdatedDirectory::Empty => None,
                }
            }
            (Some(DirectoryEntry::File(_)), _, false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (Some(DirectoryEntry::Directory(id)), Some(Some(value)), false) => {
                let old_child = store.read_node(id).await?;
                match Box::pin(update_directory(
                    store,
                    old_child,
                    child_edits,
                    pending,
                    budget,
                ))
                .await?
                {
                    UpdatedDirectory::Empty => Some(DirectoryEntry::File(value)),
                    UpdatedDirectory::Tree(_) => {
                        return Err(MerkleTreeError::FileInPath(name.clone()));
                    }
                }
            }
            (Some(DirectoryEntry::Directory(_)), Some(None), false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (Some(DirectoryEntry::Directory(id)), None, false) => {
                let old_child = store.read_node(id).await?;
                match Box::pin(update_directory(
                    store,
                    old_child,
                    child_edits,
                    pending,
                    budget,
                ))
                .await?
                {
                    UpdatedDirectory::Tree(id) => Some(DirectoryEntry::Directory(id)),
                    UpdatedDirectory::Empty => None,
                }
            }
            (None, Some(Some(_)), false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (None, _, false) => {
                let empty = DirectoryNode::default();
                match Box::pin(update_directory(store, empty, child_edits, pending, budget)).await?
                {
                    UpdatedDirectory::Tree(id) => Some(DirectoryEntry::Directory(id)),
                    UpdatedDirectory::Empty => None,
                }
            }
            (None, None, true) => None,
            (Some(entry), None, true) => Some(entry),
        };
        if let Some(replacement) = replacement {
            directory.entries.insert(name.clone(), replacement);
        }
    }

    if directory.entries.is_empty() {
        Ok(UpdatedDirectory::Empty)
    } else {
        // Charge every retained directory body against the aggregate budget
        // before it enters the pending write set, so the memory the caller
        // holds between derivation and persistence stays bounded.
        let directory = budget.stage(directory)?;
        let id = directory.id();
        pending.push(directory);
        Ok(UpdatedDirectory::Tree(id))
    }
}
