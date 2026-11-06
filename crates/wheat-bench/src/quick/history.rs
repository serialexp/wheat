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
    Timestamp, Tree, ViewId,
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

/// An object store counting the objects and bytes read through it, and the bytes
/// written to it.
pub(crate) struct Counting {
    pub(crate) inner: PackedObjectStore,
    reads: AtomicU64,
    bytes: AtomicU64,
    written: AtomicU64,
}

impl Counting {
    pub(crate) fn new(inner: PackedObjectStore) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            written: AtomicU64::new(0),
        }
    }

    /// The reads and bytes since the last call.
    pub(crate) fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }

    /// The bytes handed to the store since the last call, whether or not it held them
    /// already.
    pub(crate) fn take_written(&self) -> u64 {
        self.written.swap(0, Ordering::Relaxed)
    }
}

#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.written
            .fetch_add(canonical_bytes.len() as u64, Ordering::Relaxed);
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        let written: usize = objects.iter().map(|(_, bytes)| bytes.len()).sum();
        self.written.fetch_add(written as u64, Ordering::Relaxed);
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

/// Change number `index`'s ID: a version 7 UUID as `wheat change new` would mint it
/// for a change made `index` seconds after the first, with its random bits drawn from
/// `index`. `ChangeId::new` reads the clock, so the maps keyed by change ID, and the
/// reads a case counts, would differ with how fast the history was built.
pub(crate) fn change_id(index: usize) -> ChangeId {
    const FIRST_MILLIS: u64 = 1_760_000_000_000;
    let millis = FIRST_MILLIS + index as u64 * 1_000;
    let mut random = [0; 16];
    random[..8].copy_from_slice(&split_mix(index as u64).to_be_bytes());
    random[8..].copy_from_slice(&split_mix(!(index as u64)).to_be_bytes());
    let mut bytes = [0; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (random[0] & 0x0f);
    bytes[7] = random[1];
    bytes[8] = 0x80 | (random[2] & 0x3f);
    bytes[9..].copy_from_slice(&random[3..10]);
    ChangeId::from_bytes(bytes)
}

/// SplitMix64's output function: spreads consecutive inputs over the whole range.
fn split_mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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
                id: change_id(index),
                metadata: ChangeMetadata {
                    title: format!("Change {index}"),
                    creator: author(),
                    ..ChangeMetadata::default()
                },
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata {
                    author: author(),
                    committer: author(),
                    committed_at: committed_at(index),
                    ..RevisionMetadata::default()
                },
                edits,
            },
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}

/// When change `index`'s revision was committed: a minute after the one before.
pub(crate) fn committed_at(index: usize) -> Timestamp {
    const FIRST_SECONDS: i64 = 1_760_000_000;
    Timestamp {
        seconds: FIRST_SECONDS + index as i64 * 60,
        offset_minutes: 0,
    }
}

/// Creates change number `index`, writing `new` over `expected` at `path`, on the
/// frontier of `effective` and publishes it. Returns the new head and view.
pub(crate) async fn write_at<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    content: &ContentStore,
    (head, base, effective): (RepositoryHead, ViewId, ViewId),
    index: usize,
    path: &str,
    expected: Option<&[u8]>,
    new: &[u8],
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let value_of = |bytes: &[u8]| FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    };
    content.put(new)?;
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(value_of),
            replacement: Some(value_of(new)),
        },
    )?;
    let id = record.id();
    let (prepared, outcome) = mutator
        .create_on_frontier(
            head,
            base,
            effective,
            ChangeProposal {
                id: change_id(index),
                metadata: ChangeMetadata {
                    title: format!("Change {index}"),
                    creator: author(),
                    ..ChangeMetadata::default()
                },
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata {
                    author: author(),
                    committer: author(),
                    committed_at: committed_at(index),
                    ..RevisionMetadata::default()
                },
                edits: RevisionEdits {
                    edits: [(id, record)].into(),
                    transition: EditTransition {
                        added: [id].into(),
                        ..EditTransition::default()
                    },
                },
            },
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}
