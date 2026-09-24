//! Settle the workspace's materialized pin after `add` captures workspace paths.
//!
//! `add` authors a revision from exact workspace bytes, so the captured paths are
//! already on disk. The resulting working view can nevertheless contain content this
//! workspace never materialized: revisions another workspace published to a change
//! this one builds on, or descendant changes reapplied over the captured change.
//! Materializing those automatically would write content the author did not choose
//! into their workspace, so settlement never touches workspace files. It only
//! records, in the pinned tree, the captured paths whose raw workspace bytes are
//! exactly the resulting view's value.
//!
//! When that accounts for every difference between the old pin and the resulting
//! view, the pin moves to the resulting view and head. Otherwise the pin keeps its
//! view and head, its tree gains the settled paths, and the caller reports that
//! `materialize` is required to bring the remaining changes into the workspace.
//! That later materialization plans from the settled tree, so it installs only the
//! unmaterialized changes and authenticates the captured paths as already present.
//!
//! A captured path can also be edited by a descendant reapplied over the captured
//! change, so its working-view value differs from the workspace bytes. Such a path
//! is reported as diverged and pinned at the captured change's own applied value
//! when the workspace holds exactly that, which lets `materialize` authenticate the
//! workspace bytes and install the descendant's result over them.

use std::path::Path;

use anyhow::Result;
use wheat_core::{ApplicationResultRecord, ChangeId, DirectoryEntry, FileValue, TreeId, ViewId};
use wheat_store::{
    ApplicationMapStore, ContentStore, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore,
    RepositoryHead, RepositoryReader, TreeEdit,
};

use crate::{materialization::workspace_holds, workspace_metadata::MaterializedState};

/// The settled pin and what it could not account for.
#[derive(Debug)]
pub(crate) struct Settlement {
    pub(crate) materialized: MaterializedState,
    /// The pin names the resulting working view; no materialization is pending.
    pub(crate) complete: bool,
    /// Captured paths whose workspace value differs from the resulting view, for
    /// example because a reapplied descendant also edits them. Materialization
    /// will replace their workspace bytes.
    pub(crate) diverged: Vec<String>,
}

/// The captured change's own applied tree in the resulting view, or `None` when
/// that application is conflicted.
async fn captured_change_tree<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    objects: &O,
    applications: wheat_core::ApplicationMapId,
    change: ChangeId,
) -> Result<Option<TreeId>> {
    let Some(application) = ApplicationMapStore::new(objects)
        .get(applications, change)
        .await?
    else {
        anyhow::bail!("captured change {change} has no application in the resulting view");
    };
    Ok(match reader.application(application).await?.result {
        ApplicationResultRecord::Clean { tree } => Some(tree),
        ApplicationResultRecord::Conflicted { .. } => None,
    })
}

/// The shape of one path in a pinned tree.
enum PathShape {
    /// The path and every ancestor directory may hold a leaf.
    Leaf(Option<FileValue>),
    /// The path is a directory or crosses a file; settling it would be a shape
    /// transition that only materialization may perform.
    Obstructed,
}

async fn path_shape<O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    root: TreeId,
    components: &[String],
) -> Result<PathShape> {
    let (leaf, ancestors) = components.split_last().ok_or(MerkleTreeError::EmptyPath)?;
    let mut node = trees.read_node(root).await?;
    for component in ancestors {
        match node.entries.get(component) {
            None => return Ok(PathShape::Leaf(None)),
            Some(DirectoryEntry::File(_)) => return Ok(PathShape::Obstructed),
            Some(DirectoryEntry::Directory(child)) => node = trees.read_node(*child).await?,
        }
    }
    Ok(match node.entries.get(leaf) {
        None => PathShape::Leaf(None),
        Some(DirectoryEntry::File(value)) => PathShape::Leaf(Some(*value)),
        Some(DirectoryEntry::Directory(_)) => PathShape::Obstructed,
    })
}

/// Compute the pin that reflects `captured` workspace paths after the revision that
/// produced `resulting_view` at `resulting_head` was prepared.
///
/// New tree nodes are written to `objects`; the caller must make them durable and
/// root them before publishing workspace metadata that references them.
pub(crate) async fn settle<O: ImmutableObjectStore>(
    root: &Path,
    objects: &O,
    content: &ContentStore,
    pinned: &MaterializedState,
    resulting_head: RepositoryHead,
    resulting_view: ViewId,
    change: ChangeId,
    captured: &[String],
) -> Result<Settlement> {
    let reader = RepositoryReader::new(objects, content);
    let resulting = reader.view(resulting_view).await?;
    let target_tree = resulting.tree;
    let trees = MerkleTreeStore::new(objects);
    // Resolved on the first diverged path only.
    let mut change_tree: Option<Option<TreeId>> = None;

    let mut edits = Vec::new();
    let mut diverged = Vec::new();
    for path in captured {
        let components = path.split('/').map(str::to_owned).collect::<Vec<_>>();
        let target = path_shape(&trees, target_tree, &components).await?;
        let current = path_shape(&trees, pinned.tree, &components).await?;
        let (target, current) = match (target, current) {
            (PathShape::Leaf(target), PathShape::Leaf(current)) => (target, current),
            // Neither tree holds a leaf here, so the capture recorded nothing the pin
            // lacks at this path.
            (PathShape::Obstructed, PathShape::Obstructed) => continue,
            _ => {
                diverged.push(path.clone());
                continue;
            }
        };
        if workspace_holds(root, content, path, target)? {
            if target != current {
                edits.push(TreeEdit {
                    path: components,
                    value: target,
                });
            }
            continue;
        }
        diverged.push(path.clone());

        let change_tree = match change_tree {
            Some(tree) => tree,
            None => {
                let tree =
                    captured_change_tree(&reader, objects, resulting.applications, change).await?;
                change_tree = Some(tree);
                tree
            }
        };
        let Some(change_tree) = change_tree else {
            continue;
        };
        let PathShape::Leaf(captured_value) = path_shape(&trees, change_tree, &components).await?
        else {
            continue;
        };
        if captured_value != current && workspace_holds(root, content, path, captured_value)? {
            edits.push(TreeEdit {
                path: components,
                value: captured_value,
            });
        }
    }

    let settled_tree = trees.apply_final_edits(pinned.tree, &edits).await?;
    if settled_tree == target_tree {
        return Ok(Settlement {
            materialized: MaterializedState {
                repository_head: resulting_head.into(),
                view: resulting_view,
                tree: target_tree,
            },
            complete: true,
            diverged,
        });
    }
    Ok(Settlement {
        materialized: MaterializedState {
            tree: settled_tree,
            ..pinned.clone()
        },
        complete: false,
        diverged,
    })
}
