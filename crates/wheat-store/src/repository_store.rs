//! A repository's objects and content together, so that what a publish or fetch
//! receives can be stored in one write.

use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use thiserror::Error;
use wheat_core::{ContentChunkManifestRecord, ContentId, ObjectId, VerifiedContent};

use crate::{
    BatchObjectStore, CollectableStructure, ContentStore, ContentStoreError, ImmutableObjectStore,
    ObjectStoreError, StoredInventory, StoredObject,
    batch::stored_object,
    content_store::{ContentBackend, ContentItem, StoredContent},
};

#[derive(Debug, Error)]
pub enum RepositoryStoreError {
    #[error("object storage failed")]
    Objects(#[from] ObjectStoreError),
    #[error("content storage failed")]
    Content(#[from] ContentStoreError),
}

/// An object store and the content store that goes with it.
#[async_trait]
pub trait RepositoryStore: ImmutableObjectStore {
    /// The store's content.
    fn content_store(&self) -> ContentStore;

    /// Durably stores `objects` and `content` together: in one write where the
    /// backend keeps both in one place, otherwise content first. On success the
    /// objects are as durable as after [`ImmutableObjectStore::put_batch`] (so a
    /// store that buffers objects still needs [`ImmutableObjectStore::seal`]) and the
    /// content as after [`ContentStore::put_many_with_bases`]. Returns what the
    /// store did not hold before.
    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError>;

    /// What the store holds, listed, when it can: the pool structure GC counts
    /// ([`crate::analyze_structure`]). `None` for a store that cannot list its objects.
    fn structural_inventory(&self) -> Option<&dyn StoredInventory> {
        None
    }

    /// The store, when it can remove what no head reaches ([`crate::collect_structure`])
    /// and pin a reader's view across that. `None` for a store that cannot.
    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        None
    }
}

/// What a write added to a store: each object and content it did not hold before,
/// with the bytes the store keeps for it. A store that cannot tell gives the
/// object's canonical length or the content's logical length instead.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoredWrite {
    pub objects: Vec<(ObjectId, u64)>,
    pub content: Vec<(ContentId, u64)>,
}

impl StoredWrite {
    #[must_use]
    pub fn object_bytes(&self) -> u64 {
        self.objects.iter().map(|(_, bytes)| bytes).sum()
    }

    #[must_use]
    pub fn content_bytes(&self) -> u64 {
        self.content.iter().map(|(_, bytes)| bytes).sum()
    }

    fn append(&mut self, other: Self) {
        self.objects.extend(other.objects);
        self.content.extend(other.content);
    }
}

/// Bytes a [`RepositoryStage`] was given: objects' canonical bytes and content's
/// logical bytes, each time they were given, whether or not the store held them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceivedBytes {
    pub objects: u64,
    pub content: u64,
}

/// Writers used by a store that writes blobs or objects one at a time.
const WRITE_CONCURRENCY: usize = 8;

/// [`RepositoryStore::put_publication`] as two writes, content then objects, for
/// stores that keep them apart.
///
/// Neither store says what it added, so each item is looked up first: one
/// [`ContentStore::contains`] per content, which also skips writing what is there,
/// and one [`ImmutableObjectStore::exists_many`] for the objects.
pub async fn put_separately<O: ImmutableObjectStore + ?Sized>(
    objects_store: &O,
    content_store: &ContentStore,
    objects: &[(ObjectId, Vec<u8>)],
    content: &[ContentItem<'_>],
) -> Result<StoredWrite, RepositoryStoreError> {
    let mut write = StoredWrite::default();
    if !content.is_empty() {
        let mut seen = HashSet::with_capacity(content.len());
        let missing = content
            .iter()
            .filter(|item| seen.insert(item.id()) && !content_store.contains(item.id()))
            .copied()
            .collect::<Vec<_>>();
        content_store.put_many_with_bases(&missing, WRITE_CONCURRENCY)?;
        write.content = missing
            .iter()
            .map(|item| (item.id(), item.bytes().len() as u64))
            .collect();
    }
    if !objects.is_empty() {
        let ids = objects.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let present = objects_store.exists_many(&ids).await?;
        objects_store.put_batch(objects, WRITE_CONCURRENCY).await?;
        let mut seen = HashSet::with_capacity(objects.len());
        write.objects = objects
            .iter()
            .zip(present)
            .filter(|((id, _), present)| !present && seen.insert(*id))
            .map(|((id, bytes), _)| (*id, bytes.len() as u64))
            .collect();
    }
    Ok(write)
}

/// An object store and a content store that are kept apart, as one
/// [`RepositoryStore`].
#[derive(Clone, Debug)]
pub struct SeparateRepositoryStore<O> {
    objects: O,
    content: ContentStore,
}

impl<O> SeparateRepositoryStore<O> {
    #[must_use]
    pub fn new(objects: O, content: ContentStore) -> Self {
        Self { objects, content }
    }

    #[must_use]
    pub fn objects(&self) -> &O {
        &self.objects
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> ImmutableObjectStore for SeparateRepositoryStore<O> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.objects.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.objects.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.objects.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.objects.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.objects.seal().await
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> RepositoryStore for SeparateRepositoryStore<O> {
    fn content_store(&self) -> ContentStore {
        self.content.clone()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        put_separately(&self.objects, &self.content, objects, content).await
    }
}

/// A fork network's structural pool and the content store every network shares, as
/// one [`RepositoryStore`]: objects are kept in `pool` and content in `content`.
///
/// [`RepositoryStore::put_publication`] is each store's own write, content first, so
/// each says what it added and batches its writes as it would alone. Reads of content
/// through `pool` find none of it.
#[derive(Clone, Debug)]
pub struct NetworkStore<P, C> {
    pool: P,
    content: C,
}

impl<P, C> NetworkStore<P, C> {
    #[must_use]
    pub fn new(pool: P, content: C) -> Self {
        Self { pool, content }
    }

    #[must_use]
    pub fn pool(&self) -> &P {
        &self.pool
    }
}

#[async_trait]
impl<P: RepositoryStore, C: RepositoryStore> ImmutableObjectStore for NetworkStore<P, C> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.pool.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.pool.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.pool.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.pool.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.pool.seal().await?;
        self.content.seal().await
    }
}

#[async_trait]
impl<P: RepositoryStore, C: RepositoryStore> RepositoryStore for NetworkStore<P, C> {
    fn content_store(&self) -> ContentStore {
        self.content.content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        let mut write = StoredWrite::default();
        if !content.is_empty() {
            write.content = self.content.put_publication(&[], content).await?.content;
        }
        if !objects.is_empty() {
            write.objects = self.pool.put_publication(objects, &[]).await?.objects;
        }
        Ok(write)
    }

    fn structural_inventory(&self) -> Option<&dyn StoredInventory> {
        self.pool.structural_inventory()
    }

    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        self.pool.collectable_structure()
    }
}

#[async_trait]
impl<T: RepositoryStore + ?Sized> RepositoryStore for &T {
    fn content_store(&self) -> ContentStore {
        (**self).content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        (**self).put_publication(objects, content).await
    }
    fn structural_inventory(&self) -> Option<&dyn StoredInventory> {
        (**self).structural_inventory()
    }
    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        (**self).collectable_structure()
    }
}

#[async_trait]
impl<T: RepositoryStore + ?Sized> RepositoryStore for Arc<T> {
    fn content_store(&self) -> ContentStore {
        (**self).content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        (**self).put_publication(objects, content).await
    }
    fn structural_inventory(&self) -> Option<&dyn StoredInventory> {
        (**self).structural_inventory()
    }
    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        (**self).collectable_structure()
    }
}

#[async_trait]
impl ImmutableObjectStore for Box<dyn RepositoryStore> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        (**self).put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        (**self).put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        (**self).get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        (**self).exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        (**self).seal().await
    }
}

#[async_trait]
impl RepositoryStore for Box<dyn RepositoryStore> {
    fn content_store(&self) -> ContentStore {
        (**self).content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        (**self).put_publication(objects, content).await
    }
    fn structural_inventory(&self) -> Option<&dyn StoredInventory> {
        (**self).structural_inventory()
    }
    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        (**self).collectable_structure()
    }
}

/// What a publish or fetch receives, held until it is accepted and then stored in
/// one [`RepositoryStore::put_publication`].
///
/// Staged objects and content read as stored, through this stage and through
/// [`Self::content`]. Only once the staged objects or the staged content reach
/// the limits is anything written early, and dropping the stage stores nothing
/// more. Content written through [`Self::content`] rather than
/// [`Self::put_content`] is stored at once.
///
/// The stage counts what it was given ([`Self::received`]) and what its writes
/// added to the store ([`Self::stored`]; [`PendingPublications::store`] for what
/// [`Self::take`] took).
///
/// The stage holds its store handle: `&store` to stage over a borrowed store, or an
/// `Arc` for a stage shared with work on other threads.
pub struct RepositoryStage<S> {
    /// The buffer, which holds the store itself.
    objects: BatchObjectStore<S>,
    staged: Arc<StagedContent>,
    content: ContentStore,
    max_objects: usize,
    max_object_bytes: usize,
    max_content_bytes: usize,
    received_objects: AtomicU64,
    received_content: AtomicU64,
    stored: Mutex<StoredWrite>,
}

impl<S: RepositoryStore> RepositoryStage<S> {
    /// Holds up to `max_objects` objects of up to `max_bytes` in total, and content
    /// of up to `max_bytes`, storing everything once either is reached (so one
    /// object or one put's content may go over).
    ///
    /// # Panics
    /// Panics if either limit is zero.
    #[must_use]
    pub fn new(store: S, max_objects: usize, max_bytes: usize) -> Self {
        assert!(max_objects != 0, "stage object limit must be nonzero");
        assert!(max_bytes != 0, "stage byte limit must be nonzero");
        let staged = Arc::new(StagedContent {
            stored: store.content_store(),
            blobs: Mutex::default(),
        });
        Self {
            // The stage flushes through `put_publication`, which says what it added,
            // so the buffer is never to write on its own.
            objects: BatchObjectStore::new(store, usize::MAX, usize::MAX),
            content: staged.stored.overlaid(staged.clone()),
            staged,
            max_objects,
            max_object_bytes: max_bytes,
            max_content_bytes: max_bytes,
            received_objects: AtomicU64::new(0),
            received_content: AtomicU64::new(0),
            stored: Mutex::default(),
        }
    }

    /// The bytes given to this stage so far.
    #[must_use]
    pub fn received(&self) -> ReceivedBytes {
        ReceivedBytes {
            objects: self.received_objects.load(Ordering::Relaxed),
            content: self.received_content.load(Ordering::Relaxed),
        }
    }

    /// What this stage's writes added to the store so far.
    pub fn stored(&self) -> Result<StoredWrite, RepositoryStoreError> {
        Ok(self.lock_stored()?.clone())
    }

    fn lock_stored(&self) -> Result<std::sync::MutexGuard<'_, StoredWrite>, ContentStoreError> {
        self.stored
            .lock()
            .map_err(|_| ContentStoreError::Io(std::io::Error::other("stage counts poisoned")))
    }

    fn receive_object(&self, bytes: usize) {
        self.received_objects
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Stores everything staged once the staged objects reach a limit.
    async fn flush_full_objects(&self) -> Result<(), ObjectStoreError> {
        let (count, bytes) = self.objects.pending_size().await;
        if count < self.max_objects && bytes < self.max_object_bytes {
            return Ok(());
        }
        self.flush().await.map_err(|error| match error {
            RepositoryStoreError::Objects(error) => error,
            RepositoryStoreError::Content(error) => ObjectStoreError::Backend(error.to_string()),
        })
    }

    /// The store's content with the staged content in it.
    #[must_use]
    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    /// Stages content, each blob with the content it is likely a small change of.
    /// Stores everything staged once the content reaches the byte limit.
    pub async fn put_content(
        &self,
        items: impl IntoIterator<Item = (VerifiedContent, Option<ContentId>)>,
    ) -> Result<(), RepositoryStoreError> {
        let full = {
            let mut blobs = self.staged.lock()?;
            let mut received = 0_u64;
            for (content, base) in items {
                received += content.len() as u64;
                blobs.insert(content, base);
            }
            self.received_content.fetch_add(received, Ordering::Relaxed);
            blobs.bytes >= self.max_content_bytes
        };
        if full {
            self.flush().await?;
        }
        Ok(())
    }

    /// Stores a long blob's manifest, as received, once its parts and chunks are
    /// staged or stored: checks they hold the bytes of `id`
    /// ([`ContentStore::check_manifest`]), stores everything staged so they are
    /// stored before the manifest names them, then stores the manifest. Unlike
    /// staged content, a refused publish leaves the blob stored; it was checked,
    /// so it is harmless.
    pub async fn put_manifest(
        &self,
        id: ContentId,
        manifest: &ContentChunkManifestRecord,
    ) -> Result<(), RepositoryStoreError> {
        if self.content.contains(id) {
            return Ok(());
        }
        self.content.check_manifest(id, manifest)?;
        self.flush().await?;
        let envelope = crate::codec::encode_chunked_envelope(manifest)
            .map_err(|error| crate::content_store::codec_error(id, usize::MAX, error))?;
        let bytes = envelope.len() as u64;
        self.staged.stored.put_raw_envelope(id, envelope)?;
        self.lock_stored()?.content.push((id, bytes));
        Ok(())
    }

    /// Stores everything staged in one write.
    ///
    /// If the write fails everything stays staged, so a later call can retry it.
    #[tracing::instrument(name = "stage_flush", level = "debug", skip_all)]
    pub async fn flush(&self) -> Result<(), RepositoryStoreError> {
        let mut pending = self.objects.lock_pending().await;
        let objects = pending.batch();
        let content = self.staged.lock()?.snapshot();
        if content.is_empty() && objects.is_empty() {
            return Ok(());
        }
        let items = content
            .iter()
            .map(|(_, blob)| blob.item())
            .collect::<Vec<_>>();
        let write = self
            .objects
            .backend()
            .put_publication(&objects, &items)
            .await?;
        pending.written();
        self.staged
            .lock()?
            .written(content.iter().map(|(id, _)| *id));
        self.lock_stored()?.append(write);
        Ok(())
    }

    /// Takes everything staged out of the stage, to be stored later with other
    /// publications in one [`PendingPublications::store`].
    pub async fn take(&self) -> Result<Arc<StagedPublication>, RepositoryStoreError> {
        let mut pending = self.objects.lock_pending().await;
        let content = std::mem::take(&mut *self.staged.lock()?).blobs;
        Ok(Arc::new(StagedPublication {
            objects: pending.take(),
            content,
            stored: std::mem::take(&mut *self.lock_stored()?),
        }))
    }
}

/// What one [`RepositoryStage`] held, taken out by [`RepositoryStage::take`].
#[derive(Debug)]
pub struct StagedPublication {
    objects: BTreeMap<ObjectId, Vec<u8>>,
    content: BTreeMap<ContentId, Arc<StagedBlob>>,
    /// What the stage's flushes had added before it was taken.
    stored: StoredWrite,
}

/// Publications accepted but not yet stored, readable in the meantime through
/// [`Self::over`] and stored together by [`Self::store`].
///
/// A lookup tries each publication in turn, so it costs one map lookup per
/// publication held; a server holds one per publish waiting on a write.
#[derive(Clone, Debug, Default)]
pub struct PendingPublications {
    publications: Vec<Arc<StagedPublication>>,
}

impl PendingPublications {
    pub fn push(&mut self, publication: Arc<StagedPublication>) {
        self.publications.push(publication);
    }

    pub fn extend(&mut self, other: &Self) {
        self.publications.extend(other.publications.iter().cloned());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.publications.is_empty()
    }

    /// Stores every publication held in one write. Returns what each publication
    /// added to the store, in the order they were pushed, counting its stage's
    /// flushes too; an item two of them held counts for the first.
    #[tracing::instrument(name = "pending_store", level = "debug", skip_all)]
    pub async fn store<S: RepositoryStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<Vec<StoredWrite>, RepositoryStoreError> {
        let mut added = self
            .publications
            .iter()
            .map(|publication| publication.stored.clone())
            .collect::<Vec<_>>();
        let mut objects = BTreeMap::new();
        let mut content = BTreeMap::new();
        for publication in &self.publications {
            objects.extend(publication.objects.iter());
            content.extend(publication.content.iter());
        }
        if objects.is_empty() && content.is_empty() {
            return Ok(added);
        }
        let objects = objects
            .into_iter()
            .map(|(id, bytes)| (*id, bytes.clone()))
            .collect::<Vec<_>>();
        let content = content
            .into_values()
            .map(|blob| blob.item())
            .collect::<Vec<_>>();
        let write = store.put_publication(&objects, &content).await?;
        for (id, bytes) in write.objects {
            if let Some(holder) = self.holder(|publication| publication.objects.contains_key(&id)) {
                added[holder].objects.push((id, bytes));
            }
        }
        for (id, bytes) in write.content {
            if let Some(holder) = self.holder(|publication| publication.content.contains_key(&id)) {
                added[holder].content.push((id, bytes));
            }
        }
        Ok(added)
    }

    /// The index of the first publication `holds`.
    fn holder(&self, holds: impl Fn(&StagedPublication) -> bool) -> Option<usize> {
        self.publications
            .iter()
            .position(|publication| holds(publication))
    }

    /// `stage` with these publications beneath it: reads find what they hold, and
    /// writes go to `stage`.
    #[must_use]
    pub fn over<'a, S: RepositoryStore>(
        &self,
        stage: &'a RepositoryStage<S>,
    ) -> PendingView<'a, S> {
        let content = stage.content.overlaid(Arc::new(PendingContent {
            publications: self.publications.clone(),
            below: stage.content.clone(),
        }));
        PendingView {
            publications: self.publications.clone(),
            stage,
            content,
        }
    }
}

/// A [`RepositoryStage`] with [`PendingPublications`] beneath it; see
/// [`PendingPublications::over`].
pub struct PendingView<'a, S> {
    publications: Vec<Arc<StagedPublication>>,
    stage: &'a RepositoryStage<S>,
    content: ContentStore,
}

impl<S> PendingView<'_, S> {
    /// The stage's content with the pending content in it.
    #[must_use]
    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    fn object(&self, id: ObjectId) -> Option<&Vec<u8>> {
        self.publications
            .iter()
            .find_map(|publication| publication.objects.get(&id))
    }
}

#[async_trait]
impl<S: RepositoryStore> ImmutableObjectStore for PendingView<'_, S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        if self.object(id).is_some() {
            return Ok(());
        }
        self.stage.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.stage.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        match self.object(id) {
            Some(bytes) => stored_object(id, bytes.clone()),
            None => self.stage.get(id).await,
        }
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut results = vec![true; ids.len()];
        let (below, positions): (Vec<_>, Vec<_>) = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| self.object(**id).is_none())
            .map(|(position, id)| (*id, position))
            .unzip();
        if !below.is_empty() {
            let found = self.stage.exists_many(&below).await?;
            for (position, found) in positions.into_iter().zip(found) {
                results[position] = found;
            }
        }
        Ok(results)
    }
}

#[async_trait]
impl<S: RepositoryStore> ImmutableObjectStore for RepositoryStage<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.receive_object(canonical_bytes.len());
        self.objects.put_if_absent(id, canonical_bytes).await?;
        self.flush_full_objects().await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        _concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        // Staging copies each object into memory; there is nothing to overlap.
        self.receive_object(objects.iter().map(|(_, bytes)| bytes.len()).sum());
        for (id, bytes) in objects {
            self.objects.put_if_absent(*id, bytes).await?;
            self.flush_full_objects().await?;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.objects.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.objects.exists_many(ids).await
    }
}

#[derive(Debug)]
struct StagedBlob {
    content: VerifiedContent,
    base: Option<ContentId>,
}

impl StagedBlob {
    fn item(&self) -> ContentItem<'_> {
        ContentItem::new(self.content.borrowed(), self.base)
    }

    fn len(&self) -> usize {
        self.content.len()
    }

    fn stored(&self) -> StoredContent {
        StoredContent::Verified(self.content.clone())
    }
}

#[derive(Debug, Default)]
struct StagedBlobs {
    blobs: BTreeMap<ContentId, Arc<StagedBlob>>,
    bytes: usize,
}

impl StagedBlobs {
    fn insert(&mut self, content: VerifiedContent, base: Option<ContentId>) {
        let id = content.id();
        if self.blobs.contains_key(&id) {
            return;
        }
        self.bytes += content.len();
        self.blobs
            .insert(id, Arc::new(StagedBlob { content, base }));
    }

    fn get(&self, id: ContentId) -> Option<Arc<StagedBlob>> {
        self.blobs.get(&id).cloned()
    }

    fn snapshot(&self) -> Vec<(ContentId, Arc<StagedBlob>)> {
        self.blobs
            .iter()
            .map(|(id, blob)| (*id, blob.clone()))
            .collect()
    }

    fn written(&mut self, ids: impl IntoIterator<Item = ContentId>) {
        for id in ids {
            if let Some(blob) = self.blobs.remove(&id) {
                self.bytes -= blob.len();
            }
        }
    }
}

/// Staged content in front of the store's own: the backend of
/// [`RepositoryStage::content`].
#[derive(Debug)]
struct StagedContent {
    stored: ContentStore,
    blobs: Mutex<StagedBlobs>,
}

impl StagedContent {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StagedBlobs>, ContentStoreError> {
        self.blobs
            .lock()
            .map_err(|_| ContentStoreError::Io(std::io::Error::other("staged content poisoned")))
    }

    fn staged(&self, id: ContentId) -> Result<Option<Arc<StagedBlob>>, ContentStoreError> {
        Ok(self.lock()?.get(id))
    }
}

impl ContentBackend for StagedContent {
    fn put_many(&self, contents: &[VerifiedContent<&[u8]>]) -> Result<(), ContentStoreError> {
        self.stored.put_many(contents, WRITE_CONCURRENCY)
    }

    fn get_stored(&self, id: ContentId) -> Result<StoredContent, ContentStoreError> {
        match self.staged(id)? {
            Some(blob) => Ok(blob.stored()),
            None => self.stored.stored(id),
        }
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        match self.staged(id)? {
            Some(blob) => Ok(blob.len() as u64),
            None => self.stored.logical_length(id),
        }
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        self.stored.put_raw_envelopes(envelopes, WRITE_CONCURRENCY)
    }

    fn supports_raw_envelopes(&self) -> bool {
        self.stored.supports_raw_envelopes()
    }

    fn existing_ids(
        &self,
        candidates: &[ContentId],
    ) -> Option<Result<HashSet<ContentId>, ContentStoreError>> {
        Some(self.stored.existing_ids(candidates).and_then(|mut found| {
            let staged = self.lock()?;
            found.extend(candidates.iter().filter(|id| staged.blobs.contains_key(id)));
            Ok(found)
        }))
    }
}

/// [`PendingPublications`]' content in front of a stage's: the backend of
/// [`PendingView::content`].
#[derive(Debug)]
struct PendingContent {
    publications: Vec<Arc<StagedPublication>>,
    below: ContentStore,
}

impl PendingContent {
    fn pending(&self, id: ContentId) -> Option<&StagedBlob> {
        self.publications
            .iter()
            .find_map(|publication| publication.content.get(&id))
            .map(|blob| &**blob)
    }
}

impl ContentBackend for PendingContent {
    fn put_many(&self, contents: &[VerifiedContent<&[u8]>]) -> Result<(), ContentStoreError> {
        self.below.put_many(contents, WRITE_CONCURRENCY)
    }

    fn get_stored(&self, id: ContentId) -> Result<StoredContent, ContentStoreError> {
        match self.pending(id) {
            Some(blob) => Ok(blob.stored()),
            None => self.below.stored(id),
        }
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        match self.pending(id) {
            Some(blob) => Ok(blob.len() as u64),
            None => self.below.logical_length(id),
        }
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        self.below.put_raw_envelopes(envelopes, WRITE_CONCURRENCY)
    }

    fn supports_raw_envelopes(&self) -> bool {
        self.below.supports_raw_envelopes()
    }

    fn existing_ids(
        &self,
        candidates: &[ContentId],
    ) -> Option<Result<HashSet<ContentId>, ContentStoreError>> {
        Some(self.below.existing_ids(candidates).map(|mut found| {
            found.extend(candidates.iter().filter(|id| {
                self.publications
                    .iter()
                    .any(|publication| publication.content.contains_key(id))
            }));
            found
        }))
    }
}
