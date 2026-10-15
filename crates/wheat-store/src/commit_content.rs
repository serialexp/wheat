//! Incremental visible-content map updates for a committed revision.

use wheat_core::{ContentMapId, FileValue};

use crate::{ContentMapError, ContentMapStore, ImmutableObjectStore};

/// Updates the parent's visible-content map for changed leaves only.
///
/// A view's content map counts the tree entries naming each blob in its resulting tree, not
/// the blobs retained by revision definitions or application evidence. Each touched path
/// takes one from its old blob's count and adds one to its new blob's, so a blob still named
/// at an untouched path keeps a positive count without walking the tree.
pub(crate) async fn update_content<O: ImmutableObjectStore>(
    objects: &O,
    parent: ContentMapId,
    touched: impl IntoIterator<Item = (Option<FileValue>, Option<FileValue>)>,
) -> Result<ContentMapId, ContentMapError> {
    let mut changes = Vec::new();
    for (old, new) in touched {
        if old.map(|value| value.content) == new.map(|value| value.content) {
            continue;
        }
        if let Some(value) = old {
            changes.push((value.content, -1));
        }
        if let Some(value) = new {
            changes.push((value.content, 1));
        }
    }
    ContentMapStore::new(objects).adjust(parent, changes).await
}
