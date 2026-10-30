use std::collections::BTreeMap;

use wheat_core::{DirectoryEntry, DirectoryNode, FileValue, TreeId, validate_directory_component};

use super::{
    Directory, FinalTreeUpdateLimits, MerkleTreeError, MerkleTreeStore, ReadCount, ReadLimits,
    TreeEdit,
};
use crate::{BatchObjectStore, ImmutableObjectStore};

#[derive(Default)]
struct FinalEditNode {
    has_direct: bool,
    direct: Option<FileValue>,
    children: BTreeMap<String, FinalEditNode>,
}

/// The update's objects, held in memory until all of them are derived.
type Staged<'s, 'o, S> = MerkleTreeStore<'s, BatchObjectStore<&'o S>>;

struct Budget {
    limits: FinalTreeUpdateLimits,
}

impl Budget {
    /// Checks what the update holds staged against the limits, so the memory held
    /// between derivation and persistence stays bounded. Each directory is checked
    /// once written, so one directory's objects may pass a limit before it fails.
    async fn check<S: ImmutableObjectStore>(
        &self,
        staged: &BatchObjectStore<&S>,
    ) -> Result<(), MerkleTreeError> {
        let (nodes, bytes) = staged.pending_size().await;
        if nodes > self.limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            });
        }
        if bytes > self.limits.max_total_node_bytes {
            return Err(MerkleTreeError::StagedNodeBytesExceeded {
                size: bytes,
                limit: self.limits.max_total_node_bytes,
            });
        }
        Ok(())
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

    // Derive and check every final object before persisting any of them, so a
    // rejected shape transition cannot leave orphan tree objects: writes stay in
    // memory, read through, until the whole update is derived.
    let staged = BatchObjectStore::new(store.objects, usize::MAX, usize::MAX);
    let trees = MerkleTreeStore::new(&staged);
    let budget = Budget { limits };
    let root = trees
        .open_directory(root, ReadLimits::UNBOUNDED, &mut ReadCount::default())
        .await?;
    let result = match update_directory(&trees, root, &final_edits, &budget).await? {
        Some(id) => id,
        None => trees.empty_root().await?,
    };
    budget.check(&staged).await?;
    staged.flush().await?;
    Ok(result)
}

/// Applies `edits` to `directory` and stages it and every changed directory below
/// it; `None` when it is left empty.
async fn update_directory<S: ImmutableObjectStore>(
    trees: &Staged<'_, '_, S>,
    mut directory: Directory,
    edits: &FinalEditNode,
    budget: &Budget,
) -> Result<Option<TreeId>, MerkleTreeError> {
    let (limits, mut count) = (ReadLimits::UNBOUNDED, ReadCount::default());
    let empty = || Directory::Flat(DirectoryNode::default());
    let mut changes = BTreeMap::new();
    for (name, child_edits) in &edits.children {
        let old = trees
            .directory_get(&mut directory, name, limits, &mut count)
            .await?;
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
                Box::pin(update_directory(trees, empty(), child_edits, budget))
                    .await?
                    .map(DirectoryEntry::Directory)
            }
            (Some(DirectoryEntry::File(_)), _, false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (Some(DirectoryEntry::Directory(id)), Some(Some(value)), false) => {
                let old_child = trees.open_directory(id, limits, &mut count).await?;
                match Box::pin(update_directory(trees, old_child, child_edits, budget)).await? {
                    None => Some(DirectoryEntry::File(value)),
                    Some(_) => return Err(MerkleTreeError::FileInPath(name.clone())),
                }
            }
            (Some(DirectoryEntry::Directory(_)), Some(None), false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (Some(DirectoryEntry::Directory(id)), None, false) => {
                let old_child = trees.open_directory(id, limits, &mut count).await?;
                Box::pin(update_directory(trees, old_child, child_edits, budget))
                    .await?
                    .map(DirectoryEntry::Directory)
            }
            (None, Some(Some(_)), false) => {
                return Err(MerkleTreeError::FileInPath(name.clone()));
            }
            (None, _, false) => Box::pin(update_directory(trees, empty(), child_edits, budget))
                .await?
                .map(DirectoryEntry::Directory),
            // Nothing to change: every edit node has a direct edit or children.
            (_, None, true) => continue,
        };
        changes.insert(name.clone(), replacement);
    }
    let id = trees.write_directory(directory, changes).await?;
    budget.check(trees.objects).await?;
    Ok(id)
}
