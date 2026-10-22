//! Building histories in process, one published mutation at a time, the way
//! `wheat change new` and `wheat add` do.
//!
//! The stores defer their syncs: with a sync per publication the disk would dominate
//! and hide how the mutation itself scales.

use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use wheat_core::{
    Actor, ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditId, EditOperation, EditRecord,
    EditTransition, FileKind, FileValue, ObjectId, Repository, RevisionEdits, RevisionMetadata,
    Tree, ViewId,
};
use wheat_store::{
    ContentStore, Durability, ImmutableObjectStore, LocalPublicationStore, ObjectStoreError,
    PackedObjectStore, PersistedRepositoryMutator, PublicationStore, RepositoryHead,
    RepositoryPublisher, StoredObject,
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

/// An object store counting the objects and bytes read through it.
pub(crate) struct Counting {
    pub(crate) inner: PackedObjectStore,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    pub(crate) fn new(inner: PackedObjectStore) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// The reads and bytes since the last call.
    pub(crate) fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }
}

#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let object = self.inner.get(id).await?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(object.canonical_bytes.len() as u64, Ordering::Relaxed);
        Ok(object)
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.inner.seal().await
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

/// Where change number `index` of a history built by [`create_at`] puts its file.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Layout {
    /// Two directories deep, so a history's files spread over 65,536 directories.
    Spread,
    /// All in one directory.
    Flat,
}

impl Layout {
    fn path(self, index: usize) -> String {
        match self {
            Self::Spread => format!(
                "d{:02x}/d{:02x}/f{index}.txt",
                index & 0xff,
                (index >> 8) & 0xff
            ),
            Self::Flat => format!("flat/f{index}.txt"),
        }
    }
}

/// Creates change number `index` on the frontier of `effective` and publishes it, the
/// way `wheat change new` does: one new file two directories deep, so a history's files
/// spread over 65,536 directories. Returns the new head and view.
pub(crate) async fn create_spread<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    content: &ContentStore,
    head: RepositoryHead,
    base: ViewId,
    effective: ViewId,
    index: usize,
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    create_at(
        mutator,
        content,
        head,
        base,
        effective,
        index,
        Layout::Spread,
    )
    .await
}

/// Creates change number `index` on the frontier of `effective` and publishes it, the
/// way `wheat change new` does, adding one file where `layout` puts it. Returns the new
/// head and view.
pub(crate) async fn create_at<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    content: &ContentStore,
    head: RepositoryHead,
    base: ViewId,
    effective: ViewId,
    index: usize,
    layout: Layout,
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let path = layout.path(index);
    let (_, edits) = rewrite(content, &path, path.as_bytes(), None)?;
    let (prepared, outcome) = mutator
        .create_on_frontier(
            head,
            base,
            effective,
            ChangeProposal {
                id: ChangeId::new(),
                metadata: ChangeMetadata {
                    title: format!("Change {index}"),
                    creator: author(),
                    ..ChangeMetadata::default()
                },
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata {
                    author: author(),
                    committer: author(),
                    ..RevisionMetadata::default()
                },
                edits,
            },
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}
