//! Exact visible-content updates without rebuilding the parent's persistent set.
use crate::{
    CommitWriterError, ContentSetStore, ImmutableObjectStore, MerkleTreeStore, ObjectKind,
    ObjectStoreError,
};
use std::collections::BTreeSet;
use wheat_core::{
    ContentManifestId, ContentSetId, DirectoryEntry, FileEdit, TreeId,
    decode_content_manifest_record,
};

pub(crate) async fn update_content<O: ImmutableObjectStore>(
    objects: &O,
    parent: ContentManifestId,
    tree: TreeId,
    edits: &[FileEdit],
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
    let additions = edits
        .iter()
        .filter_map(|e| e.replacement)
        .collect::<BTreeSet<_>>();
    let mut removals = edits
        .iter()
        .filter_map(|e| e.expected)
        .filter(|id| !additions.contains(id))
        .collect::<BTreeSet<_>>();
    // Content can appear at multiple paths. Only remove an old ID if no file
    // in the resulting tree still uses it. One shared scan checks all candidates.
    if !removals.is_empty() {
        let trees = MerkleTreeStore::new(objects);
        let mut stack = vec![trees.read_node(tree).await?.entries.into_values()];
        while let Some(frame) = stack.last_mut() {
            match frame.next() {
                None => {
                    stack.pop();
                }
                Some(DirectoryEntry::File(id)) => {
                    removals.remove(&id);
                    if removals.is_empty() {
                        break;
                    }
                }
                Some(DirectoryEntry::Directory(id)) => {
                    if stack.len() >= 1024 {
                        return Err(CommitWriterError::InvalidPath(
                            "content scan exceeds directory depth limit".into(),
                        ));
                    }
                    stack.push(trees.read_node(id).await?.entries.into_values());
                }
            }
        }
    }
    if additions.is_empty() && removals.is_empty() {
        return Ok(root);
    }
    Ok(ContentSetStore::new(objects)
        .update_batch(
            root,
            removals
                .into_iter()
                .map(|id| (id, false))
                .chain(additions.into_iter().map(|id| (id, true))),
        )
        .await?)
}
