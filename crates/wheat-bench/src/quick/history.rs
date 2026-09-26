//! Building histories in process, one published mutation at a time, the way
//! `wheat change new` and `wheat add` do.
//!
//! The stores defer their syncs: with a sync per publication the disk would dominate
//! and hide how the mutation itself scales.

use std::path::Path;

use anyhow::Result;
use wheat_core::{
    Actor, ContentId, EditId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    Repository, RevisionEdits, Tree, ViewId,
};
use wheat_store::{
    ContentStore, Durability, LocalPublicationStore, PackedObjectStore, PersistedRepositoryMutator,
    RepositoryHead, RepositoryPublisher,
};

/// A scratch repository whose stores defer their syncs.
pub(crate) struct Scratch {
    pub(crate) objects: PackedObjectStore,
    pub(crate) content: ContentStore,
    pub(crate) heads: LocalPublicationStore,
}

impl Scratch {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            objects: PackedObjectStore::open_deferred(root.join("objects"))?,
            content: ContentStore::with_durability(root.join("content"), Durability::Deferred),
            heads: LocalPublicationStore::with_durability(
                root.join("repository.head"),
                Durability::Deferred,
            ),
        })
    }

    /// Publishes an empty repository; returns its base view and head.
    pub(crate) async fn initialize(&self) -> Result<(ViewId, RepositoryHead)> {
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let head = RepositoryPublisher::new(&self.objects, &self.content, &self.heads)
            .initialize(&repository)
            .await?;
        Ok((base, head))
    }

    pub(crate) fn mutator(
        &self,
    ) -> PersistedRepositoryMutator<'_, PackedObjectStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }
}

pub(crate) fn author() -> Actor {
    Actor {
        name: "Wheat Bench".into(),
        email: "wheat-bench@example.invalid".into(),
    }
}

/// Stores `value` and returns an edit that sets `path` to it, replacing `previous`
/// (the edit an earlier checkpoint of the same change wrote to `path`).
pub(crate) fn rewrite(
    content: &ContentStore,
    path: &str,
    value: &[u8],
    previous: Option<EditId>,
) -> Result<(EditId, RevisionEdits)> {
    content.put(value)?;
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )?;
    let id = record.id();
    Ok((
        id,
        RevisionEdits {
            edits: [(id, record)].into(),
            transition: EditTransition {
                added: [id].into(),
                removed: previous.into_iter().collect(),
                ..EditTransition::default()
            },
        },
    ))
}
