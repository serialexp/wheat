//! Incremental visible-content manifest updates for a committed revision.

use std::collections::BTreeSet;

use wheat_core::{
    ContentId, ContentManifestId, ContentSetId, DirectoryEntry, FileValue, TreeId,
    decode_content_manifest_record,
};

use crate::{
    CommitWriterError, ContentSetStore, ImmutableObjectStore, MerkleTreeStore, ObjectKind,
    ObjectStoreError,
};

/// Updates the parent's visible-content manifest for changed leaves only.
///
/// A view manifest describes exactly the files visible from its resulting tree, not the blobs
/// retained by revision definitions or application evidence.  Adding/replacing a leaf is a radix
/// set update.  We only walk the resulting Merkle tree when an old ID might be removed: identical
/// content can occur at another (untouched) path, in which case it remains visible.
pub(crate) async fn update_content<O: ImmutableObjectStore>(
    objects: &O,
    parent: ContentManifestId,
    resulting_tree: TreeId,
    touched: impl IntoIterator<Item = (Option<FileValue>, Option<FileValue>)>,
) -> Result<ContentSetId, CommitWriterError> {
    let object = objects.get(parent.into()).await?;
    if object.kind != ObjectKind::ContentManifest {
        return Err(ObjectStoreError::WrongKind {
            id: parent.into(),
            expected: ObjectKind::ContentManifest,
            actual: object.kind,
        }
        .into());
    }
    let root = decode_content_manifest_record(&object.canonical_bytes)?.content;
    let mut additions = BTreeSet::new();
    let mut possibly_removed = BTreeSet::new();
    for (old, new) in touched {
        if old == new {
            continue;
        }
        if let Some(value) = new {
            additions.insert(value.content);
        }
        if let Some(value) = old {
            possibly_removed.insert(value.content);
        }
    }
    if additions.is_empty() && possibly_removed.is_empty() {
        return Ok(root);
    }

    // A candidate can have been inserted at another touched path, or can remain in an untouched
    // subtree.  Do not remove it until the latter has been ruled out by authenticated traversal.
    let trees = MerkleTreeStore::new(objects);
    let mut removals = BTreeSet::new();
    for content in possibly_removed {
        if !additions.contains(&content)
            && !tree_contains_content(&trees, resulting_tree, content).await?
        {
            removals.insert(content);
        }
    }
    Ok(ContentSetStore::new(objects)
        .update_batch(
            root,
            additions
                .into_iter()
                .map(|id| (id, true))
                .chain(removals.into_iter().map(|id| (id, false))),
        )
        .await?)
}

async fn tree_contains_content<O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    root: TreeId,
    wanted: ContentId,
) -> Result<bool, CommitWriterError> {
    for entry in trees.read_node(root).await?.entries.values() {
        match entry {
            DirectoryEntry::File(value) if value.content == wanted => return Ok(true),
            DirectoryEntry::Directory(child) => {
                if Box::pin(tree_contains_content(trees, *child, wanted)).await? {
                    return Ok(true);
                }
            }
            DirectoryEntry::File(_) => {}
        }
    }
    Ok(false)
}
