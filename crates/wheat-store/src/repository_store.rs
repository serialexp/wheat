//! A repository's objects and content together, so that what a publish or fetch
//! receives can be stored in one write.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use thiserror::Error;
use wheat_core::{ContentId, ObjectId};

use crate::{
    BatchObjectStore, ContentStore, ContentStoreError, ImmutableObjectStore, ObjectStoreError,
    StoredObject,
    batch::stored_object,
    codec::{Codec, EnvelopeHeader},
    content_store::ContentBackend,
};

/// Content to store, with the content it is likely a small change of.
#[derive(Clone, Copy, Debug)]
pub struct ContentItem<'a> {
    pub id: ContentId,
    pub bytes: &'a [u8],
    /// Content to try storing `bytes` as a delta against.
    pub base: Option<ContentId>,
}

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
    /// content as after [`ContentStore::put_many_with_bases`].
    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<(), RepositoryStoreError>;
}

/// Writers used by a store that writes blobs or objects one at a time.
const WRITE_CONCURRENCY: usize = 8;

/// [`RepositoryStore::put_publication`] as two writes, content then objects, for
/// stores that keep them apart.
pub async fn put_separately<O: ImmutableObjectStore + ?Sized>(
    objects_store: &O,
    content_store: &ContentStore,
    objects: &[(ObjectId, Vec<u8>)],
    content: &[ContentItem<'_>],
) -> Result<(), RepositoryStoreError> {
    if !content.is_empty() {
        content_store.put_many_with_bases(
            content
                .iter()
                .map(|item| (item.id, item.bytes.to_vec(), item.base)),
            WRITE_CONCURRENCY,
        )?;
    }
    if !objects.is_empty() {
        objects_store.put_batch(objects, WRITE_CONCURRENCY).await?;
    }
    Ok(())
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
    ) -> Result<(), RepositoryStoreError> {
        put_separately(&self.objects, &self.content, objects, content).await
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
    ) -> Result<(), RepositoryStoreError> {
        (**self).put_publication(objects, content).await
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
pub struct RepositoryStage<'a, S: ?Sized> {
    store: &'a S,
    objects: BatchObjectStore<'a, S>,
    staged: Arc<StagedContent>,
    content: ContentStore,
    max_content_bytes: usize,
}

impl<'a, S: RepositoryStore + ?Sized> RepositoryStage<'a, S> {
    /// Holds up to `max_objects` objects of up to `max_bytes` in total, and content
    /// of up to `max_bytes`.
    ///
    /// # Panics
    /// Panics if either limit is zero.
    #[must_use]
    pub fn new(store: &'a S, max_objects: usize, max_bytes: usize) -> Self {
        let staged = Arc::new(StagedContent {
            stored: store.content_store(),
            blobs: Mutex::default(),
        });
        Self {
            store,
            objects: BatchObjectStore::new(store, max_objects, max_bytes),
            content: staged.stored.overlaid(staged.clone()),
            staged,
            max_content_bytes: max_bytes,
        }
    }

    /// The store's content with the staged content in it.
    #[must_use]
    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    /// Stages content, checking each blob's identity. Stores everything staged
    /// once the content reaches the byte limit.
    pub async fn put_content(
        &self,
        items: impl IntoIterator<Item = (ContentId, Vec<u8>, Option<ContentId>)>,
    ) -> Result<(), RepositoryStoreError> {
        let full = {
            let mut blobs = self.staged.lock()?;
            for (id, bytes, base) in items {
                let actual = ContentId::of(&bytes);
                if actual != id {
                    return Err(ContentStoreError::Corrupt {
                        expected: id,
                        actual,
                    }
                    .into());
                }
                blobs.insert(id, bytes, base);
            }
            blobs.bytes >= self.max_content_bytes
        };
        if full {
            self.flush().await?;
        }
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
            .map(|(id, blob)| ContentItem {
                id: *id,
                bytes: &blob.bytes,
                base: blob.base,
            })
            .collect::<Vec<_>>();
        self.store.put_publication(&objects, &items).await?;
        pending.written();
        self.staged
            .lock()?
            .written(content.iter().map(|(id, _)| *id));
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
        }))
    }
}

/// What one [`RepositoryStage`] held, taken out by [`RepositoryStage::take`].
#[derive(Debug)]
pub struct StagedPublication {
    objects: BTreeMap<ObjectId, Vec<u8>>,
    content: BTreeMap<ContentId, Arc<StagedBlob>>,
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

    /// Stores every publication held in one write.
    #[tracing::instrument(name = "pending_store", level = "debug", skip_all)]
    pub async fn store<S: RepositoryStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<(), RepositoryStoreError> {
        let mut objects = BTreeMap::new();
        let mut content = BTreeMap::new();
        for publication in &self.publications {
            objects.extend(publication.objects.iter());
            content.extend(publication.content.iter());
        }
        if objects.is_empty() && content.is_empty() {
            return Ok(());
        }
        let objects = objects
            .into_iter()
            .map(|(id, bytes)| (*id, bytes.clone()))
            .collect::<Vec<_>>();
        let content = content
            .into_iter()
            .map(|(id, blob)| ContentItem {
                id: *id,
                bytes: &blob.bytes,
                base: blob.base,
            })
            .collect::<Vec<_>>();
        store.put_publication(&objects, &content).await
    }

    /// `stage` with these publications beneath it: reads find what they hold, and
    /// writes go to `stage`.
    #[must_use]
    pub fn over<'a, 's, S: RepositoryStore + ?Sized>(
        &self,
        stage: &'a RepositoryStage<'s, S>,
    ) -> PendingView<'a, 's, S> {
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
pub struct PendingView<'a, 's, S: ?Sized> {
    publications: Vec<Arc<StagedPublication>>,
    stage: &'a RepositoryStage<'s, S>,
    content: ContentStore,
}

impl<S: ?Sized> PendingView<'_, '_, S> {
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
impl<S: RepositoryStore + ?Sized> ImmutableObjectStore for PendingView<'_, '_, S> {
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
impl<S: RepositoryStore + ?Sized> ImmutableObjectStore for RepositoryStage<'_, S> {
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
}

#[derive(Debug)]
struct StagedBlob {
    bytes: Vec<u8>,
    base: Option<ContentId>,
}

impl StagedBlob {
    fn bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        if self.bytes.len() > max {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: self.bytes.len(),
            });
        }
        Ok(self.bytes.clone())
    }

    fn raw_envelope(&self) -> (EnvelopeHeader, Vec<u8>) {
        (
            EnvelopeHeader {
                codec: Codec::Raw,
                stored_length: self.bytes.len() as u64,
                logical_length: self.bytes.len() as u64,
            },
            self.bytes.clone(),
        )
    }
}

#[derive(Debug, Default)]
struct StagedBlobs {
    blobs: BTreeMap<ContentId, Arc<StagedBlob>>,
    bytes: usize,
}

impl StagedBlobs {
    fn insert(&mut self, id: ContentId, bytes: Vec<u8>, base: Option<ContentId>) {
        if self.blobs.contains_key(&id) {
            return;
        }
        self.bytes += bytes.len();
        self.blobs.insert(id, Arc::new(StagedBlob { bytes, base }));
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
                self.bytes -= blob.bytes.len();
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
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        self.stored
            .put_many(contents.iter().cloned(), WRITE_CONCURRENCY)
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        match self.staged(id)? {
            Some(blob) => blob.bounded(id, max),
            None => self.stored.get_bounded(id, max),
        }
    }

    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(EnvelopeHeader, Vec<u8>), ContentStoreError> {
        match self.staged(id)? {
            Some(blob) => Ok(blob.raw_envelope()),
            None => self.stored.raw_envelope(id),
        }
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        match self.staged(id)? {
            Some(blob) => Ok(blob.bytes.len() as u64),
            None => self.stored.logical_length(id),
        }
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        for (id, envelope) in envelopes {
            self.stored.put_raw_envelope(*id, envelope.clone())?;
        }
        Ok(())
    }

    fn supports_raw_envelopes(&self) -> bool {
        self.stored.supports_raw_envelopes()
    }

    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        let stored = self.stored.list_ids()?;
        Some(stored.and_then(|mut ids| {
            ids.extend(self.lock()?.blobs.keys().copied());
            Ok(ids)
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
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        self.below
            .put_many(contents.iter().cloned(), WRITE_CONCURRENCY)
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        match self.pending(id) {
            Some(blob) => blob.bounded(id, max),
            None => self.below.get_bounded(id, max),
        }
    }

    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(EnvelopeHeader, Vec<u8>), ContentStoreError> {
        match self.pending(id) {
            Some(blob) => Ok(blob.raw_envelope()),
            None => self.below.raw_envelope(id),
        }
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        match self.pending(id) {
            Some(blob) => Ok(blob.bytes.len() as u64),
            None => self.below.logical_length(id),
        }
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        for (id, envelope) in envelopes {
            self.below.put_raw_envelope(*id, envelope.clone())?;
        }
        Ok(())
    }

    fn supports_raw_envelopes(&self) -> bool {
        self.below.supports_raw_envelopes()
    }

    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        let below = self.below.list_ids()?;
        Some(below.map(|mut ids| {
            for publication in &self.publications {
                ids.extend(publication.content.keys().copied());
            }
            ids
        }))
    }
}
