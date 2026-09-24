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

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::Result;
use wheat_core::{ApplicationResultRecord, ChangeId, FileValue, TreeId, ViewId};
use wheat_store::{
    ApplicationMapStore, ContentStore, ImmutableObjectStore, MerkleTreeStore, RepositoryHead,
    RepositoryReader, TreeEdit, TreePathShape,
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
#[derive(Clone, Copy)]
enum PathShape {
    /// The path and every ancestor directory may hold a leaf.
    Leaf(Option<FileValue>),
    /// The path is a directory or crosses a file; settling it would be a shape
    /// transition that only materialization may perform.
    Obstructed,
}

/// Classifies `paths` (sorted and unique) in one traversal of `root`; a lookup per path
/// would decode a shared directory once for every captured file below it.
async fn path_shapes<'p, O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    root: TreeId,
    paths: &[&'p str],
) -> Result<BTreeMap<&'p str, PathShape>> {
    let shapes = trees.path_shapes(root, paths).await?;
    Ok(paths
        .iter()
        .copied()
        .zip(shapes.into_iter().map(|shape| match shape {
            TreePathShape::Absent => PathShape::Leaf(None),
            TreePathShape::File(value) => PathShape::Leaf(Some(value)),
            TreePathShape::Directory | TreePathShape::BelowFile { .. } => PathShape::Obstructed,
        }))
        .collect())
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
    let sorted = captured
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let targets = path_shapes(&trees, target_tree, &sorted).await?;
    let currents = path_shapes(&trees, pinned.tree, &sorted).await?;

    let mut edits = Vec::new();
    let mut diverged = Vec::new();
    // Diverged leaves the workspace still holds, with their pinned value; they are settled
    // at the captured change's own value below.
    let mut unsettled = Vec::new();
    for path in captured {
        let (target, current) = match (targets[path.as_str()], currents[path.as_str()]) {
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
                    path: path.split('/').map(str::to_owned).collect(),
                    value: target,
                });
            }
            continue;
        }
        diverged.push(path.clone());
        unsettled.push((path.as_str(), current));
    }

    // The captured change's tree is resolved only when a path diverged.
    let change_tree = if unsettled.is_empty() {
        None
    } else {
        captured_change_tree(&reader, objects, resulting.applications, change).await?
    };
    if let Some(change_tree) = change_tree {
        let mut paths = unsettled.iter().map(|(path, _)| *path).collect::<Vec<_>>();
        paths.sort_unstable();
        let captured_values = path_shapes(&trees, change_tree, &paths).await?;
        for (path, current) in unsettled {
            let PathShape::Leaf(captured_value) = captured_values[path] else {
                continue;
            };
            if captured_value != current && workspace_holds(root, content, path, captured_value)? {
                edits.push(TreeEdit {
                    path: path.split('/').map(str::to_owned).collect(),
                    value: captured_value,
                });
            }
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
