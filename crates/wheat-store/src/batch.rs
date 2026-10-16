use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::{Mutex, MutexGuard};
use wheat_core::{ObjectId, validate_canonical_object};

use crate::{ImmutableObjectStore, ObjectStoreError, StoredObject};

const DEFAULT_CONCURRENCY: usize = 16;
const MAX_CONCURRENCY: usize = 16;

#[derive(Debug, Default)]
struct Pending {
    objects: BTreeMap<ObjectId, Vec<u8>>,
    bytes: usize,
}

impl Pending {
    fn batch(&self) -> Vec<(ObjectId, Vec<u8>)> {
        self.objects
            .iter()
            .map(|(id, bytes)| (*id, bytes.clone()))
            .collect()
    }

    fn clear(&mut self) {
        self.objects.clear();
        self.bytes = 0;
    }
}

/// The buffered objects of a [`BatchObjectStore`], held so the caller can write them
/// itself. Nothing is buffered or read through meanwhile.
pub(crate) struct PendingObjects<'a>(MutexGuard<'a, Pending>);

impl PendingObjects<'_> {
    /// The buffered objects, to write.
    pub(crate) fn batch(&self) -> Vec<(ObjectId, Vec<u8>)> {
        self.0.batch()
    }

    /// Forgets the buffered objects once the caller has made them durable.
    pub(crate) fn written(&mut self) {
        self.0.clear();
    }
}

/// A bounded, read-through write buffer for an immutable object store.
///
/// Writes are visible through this wrapper immediately, but are not durable
/// until an automatic or explicit [`Self::flush`] completes successfully.
/// Dropping the wrapper does not flush it.
#[derive(Debug)]
pub struct BatchObjectStore<'a, O: ?Sized> {
    backend: &'a O,
    max_objects: usize,
    max_bytes: usize,
    concurrency: usize,
    pending: Mutex<Pending>,
}

impl<'a, O: ImmutableObjectStore + ?Sized> BatchObjectStore<'a, O> {
    /// Creates a buffer using up to 16 concurrent backend writes.
    ///
    /// # Panics
    /// Panics if either limit is zero.
    #[must_use]
    pub fn new(backend: &'a O, max_objects: usize, max_bytes: usize) -> Self {
        Self::with_concurrency(backend, max_objects, max_bytes, DEFAULT_CONCURRENCY)
    }

    /// Creates a buffer with an explicit backend-write concurrency.
    ///
    /// Concurrency is capped at 16 to keep flush resource use bounded.
    ///
    /// # Panics
    /// Panics if a limit or `concurrency` is zero.
    #[must_use]
    pub fn with_concurrency(
        backend: &'a O,
        max_objects: usize,
        max_bytes: usize,
        concurrency: usize,
    ) -> Self {
        assert!(max_objects != 0, "batch object limit must be nonzero");
        assert!(max_bytes != 0, "batch byte limit must be nonzero");
        assert!(concurrency != 0, "batch concurrency must be nonzero");
        Self {
            backend,
            max_objects,
            max_bytes,
            concurrency: concurrency.min(MAX_CONCURRENCY),
            pending: Mutex::new(Pending::default()),
        }
    }

    /// Makes every currently buffered object durable in the underlying store.
    ///
    /// If any backend write fails, the entire batch remains buffered so that a
    /// later call can retry it idempotently. Some objects may already have been
    /// durably written by the failed attempt.
    #[tracing::instrument(name = "object_flush", level = "debug", skip_all)]
    pub async fn flush(&self) -> Result<(), ObjectStoreError> {
        let mut pending = self.pending.lock().await;
        self.flush_locked(&mut pending).await
    }

    /// Holds the buffered objects for a write the caller makes instead of
    /// [`Self::flush`].
    pub(crate) async fn lock_pending(&self) -> PendingObjects<'_> {
        PendingObjects(self.pending.lock().await)
    }

    async fn flush_locked(&self, pending: &mut Pending) -> Result<(), ObjectStoreError> {
        if pending.objects.is_empty() {
            return Ok(());
        }
        self.backend
            .put_batch(&pending.batch(), self.concurrency)
            .await?;
        pending.clear();
        Ok(())
    }

    fn stored(id: ObjectId, canonical_bytes: Vec<u8>) -> Result<StoredObject, ObjectStoreError> {
        let actual = ObjectId::of_canonical_bytes(&canonical_bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        let kind = validate_canonical_object(&canonical_bytes)?.into();
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: canonical_bytes.into(),
        })
    }
}

#[async_trait]
impl<O: ImmutableObjectStore + ?Sized> ImmutableObjectStore for BatchObjectStore<'_, O> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        validate_canonical_object(canonical_bytes)?;
        let actual = ObjectId::of_canonical_bytes(canonical_bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }

        let mut pending = self.pending.lock().await;
        let old_len = pending.objects.get(&id).map_or(0, Vec::len);
        let new_bytes = pending
            .bytes
            .checked_sub(old_len)
            .and_then(|bytes| bytes.checked_add(canonical_bytes.len()))
            .expect("pending byte accounting overflow");
        let new_count = pending.objects.len() + usize::from(!pending.objects.contains_key(&id));

        // An object larger than the byte budget is never retained. Preserve
        // write ordering by first making all older buffered writes durable.
        if canonical_bytes.len() > self.max_bytes {
            self.flush_locked(&mut pending).await?;
            return self.backend.put_if_absent(id, canonical_bytes).await;
        }

        // Flush before insertion so the configured bounds are never exceeded.
        if new_count > self.max_objects || new_bytes > self.max_bytes {
            self.flush_locked(&mut pending).await?;
        }

        if let Some(previous) = pending.objects.insert(id, canonical_bytes.to_vec()) {
            pending.bytes -= previous.len();
        }
        pending.bytes += canonical_bytes.len();

        // Reaching either limit triggers durable completion of this put.
        if pending.objects.len() == self.max_objects || pending.bytes == self.max_bytes {
            self.flush_locked(&mut pending).await?;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let pending = self.pending.lock().await;
        if let Some(bytes) = pending.objects.get(&id) {
            return Self::stored(id, bytes.clone());
        }
        // Keep the lock while reading through so a concurrent put cannot create
        // an ordering-dependent false Missing result.
        self.backend.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let pending = self.pending.lock().await;
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(bytes) = pending.objects.get(id) {
                Self::stored(*id, bytes.clone())?;
                results.push(true);
            } else {
                results.push(match self.backend.get(*id).await {
                    Ok(_) => true,
                    Err(ObjectStoreError::Missing(_)) => false,
                    Err(error) => return Err(error),
                });
            }
        }
        Ok(results)
    }
}
