//! Public object/content adapters backed by immutable packed segments.
use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use wheat_core::{ContentId, ObjectId, validate_canonical_object};

use super::{
    catalog::{CatalogError, PackedCatalog},
    segment::{CONTENT_NAMESPACE, METADATA_NAMESPACE, SegmentRecord},
    trace::{PackedStoreTrace, PackedStoreTraceCounters},
};
use crate::content_store::ContentBackend;
use crate::{
    ContentStore, ContentStoreError, Durability, ImmutableObjectStore, LooseObjectStore,
    ObjectKind, ObjectStoreError, StoredObject,
};

const DEFAULT_LOOSE_THRESHOLD: usize = 128 * 1024;

#[derive(Clone)]
pub struct PackedObjectStore {
    engine: Arc<PackedEngine>,
}

struct PackedEngine {
    catalog: Arc<Mutex<PackedCatalog>>,
    objects: LooseObjectStore,
    content: ContentStore,
    content_root: PathBuf,
    loose_threshold: usize,
    durability: Durability,
    trace: Option<Arc<PackedStoreTraceCounters>>,
}

impl fmt::Debug for PackedObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedObjectStore")
            .field("loose_threshold", &self.engine.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PackedEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedEngine")
            .field("loose_threshold", &self.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl PackedObjectStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Durable, false)
    }

    /// Opens storage for an isolated transaction whose entire directory remains
    /// invisible until [`crate::sync_filesystem`] and atomic installation.
    pub fn open_deferred(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, false)
    }

    /// Opens deferred storage with constant-memory diagnostic counters enabled.
    pub fn open_deferred_traced(
        root: impl AsRef<Path>,
    ) -> Result<(Self, PackedStoreTrace), ObjectStoreError> {
        let store =
            Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, true)?;
        let trace = PackedStoreTrace {
            counters: Arc::downgrade(store.engine.trace.as_ref().expect("trace was enabled")),
        };
        Ok((store, trace))
    }

    pub fn open_with_loose_threshold(
        root: impl AsRef<Path>,
        loose_threshold: usize,
    ) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, loose_threshold, Durability::Durable, false)
    }

    fn open_with_options(
        root: impl AsRef<Path>,
        loose_threshold: usize,
        durability: Durability,
        collect_trace: bool,
    ) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        let trace = collect_trace.then(|| Arc::new(PackedStoreTraceCounters::default()));
        let catalog = if let Some(trace) = trace.clone() {
            PackedCatalog::open_with_trace(root.join("packed"), durability, trace)
        } else {
            PackedCatalog::open(root.join("packed"), durability)
        }
        .map_err(object_catalog_error)?;
        Ok(Self {
            engine: Arc::new(PackedEngine {
                catalog: Arc::new(Mutex::new(catalog)),
                objects: LooseObjectStore::with_durability(root.join("loose-objects"), durability),
                content: ContentStore::with_durability(root.join("loose-content"), durability),
                content_root: root.join("loose-content"),
                loose_threshold,
                durability,
                trace,
            }),
        })
    }

    #[must_use]
    pub fn content_store(&self) -> ContentStore {
        ContentStore::with_backend(
            self.engine.content_root.clone(),
            self.engine.clone(),
            self.engine.durability,
        )
    }

    /// Explicitly closes every packed-store file handle and releases its process lock.
    ///
    /// Closing consumes this handle and fails while another object-store clone or a content
    /// adapter still references the same engine. Callers performing a terminal filesystem sync
    /// must drop those handles first so close-time SQLite errors are observable before the sync.
    #[cfg(test)]
    pub(crate) fn test_fail_close_before_finalize(&self) -> Result<(), ObjectStoreError> {
        self.catalog()?
            .test_set_close_failpoint(super::catalog::CloseFailpoint::BeforeFinalize);
        Ok(())
    }

    pub fn close(self) -> Result<(), ObjectStoreError> {
        self.close_with_trace().map(|_| ())
    }

    /// Closes the store and returns its final trace snapshot when tracing was enabled.
    pub fn close_with_trace(
        self,
    ) -> Result<Option<super::PackedStoreTraceSnapshot>, ObjectStoreError> {
        let engine = Arc::try_unwrap(self.engine).map_err(|_| {
            ObjectStoreError::Backend(
                "StillReferenced: packed object/content store handles remain live".into(),
            )
        })?;
        let catalog = Arc::try_unwrap(engine.catalog)
            .map_err(|_| {
                ObjectStoreError::Backend(
                    "StillReferenced: packed catalog handle remains live".into(),
                )
            })?
            .into_inner()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))?;
        catalog.close().map_err(object_catalog_error)?;
        Ok(engine.trace.map(|trace| trace.snapshot()))
    }

    fn catalog(&self) -> Result<MutexGuard<'_, PackedCatalog>, ObjectStoreError> {
        self.engine
            .catalog
            .lock()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))
    }
}

fn object_catalog_error(error: CatalogError) -> ObjectStoreError {
    match error {
        CatalogError::Io(error) => ObjectStoreError::Io(error),
        error => ObjectStoreError::Backend(error.to_string()),
    }
}
fn content_catalog_error(error: CatalogError) -> ContentStoreError {
    match error {
        CatalogError::Io(error) => ContentStoreError::Io(error),
        error => ContentStoreError::Io(io::Error::other(error.to_string())),
    }
}

#[async_trait]
impl ImmutableObjectStore for PackedObjectStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.put_batch(&[(id, bytes.to_vec())], 1).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        if let Some(trace) = &self.engine.trace {
            trace.object_put_batch(
                objects.len(),
                objects.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut unique = BTreeMap::new();
        for (id, bytes) in objects {
            let canonical_kind: ObjectKind = validate_canonical_object(bytes)?.into();
            let actual = ObjectId::of_canonical_bytes(bytes);
            if actual != *id {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if let Some(previous) = unique.insert(*id, (canonical_kind, bytes))
                && previous.1 != bytes
            {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
        }
        let mut packed = Vec::new();
        let mut loose = Vec::new();
        for (id, (kind, bytes)) in unique {
            if bytes.len() > self.engine.loose_threshold {
                loose.push((id, bytes.clone()));
            } else {
                packed.push((id, kind, bytes));
            }
        }
        if !loose.is_empty() {
            self.engine.objects.put_batch(&loose, concurrency).await?;
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, kind, data)| SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind: *kind as u16,
                    digest: *id.as_bytes(),
                    bytes: data,
                })
                .collect::<Vec<_>>();
            self.catalog()?
                .put_records(&records)
                .map_err(object_catalog_error)?;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let Some((stored_kind, bytes)) = self
            .catalog()?
            .get(METADATA_NAMESPACE, *id.as_bytes())
            .map_err(object_catalog_error)?
        else {
            // Oversized metadata bypasses packed segments. Probe the loose
            // fallback only after the in-memory locator says the ID is not packed;
            // the common packed path must not issue one failed filesystem lookup
            // per logical object read.
            let object = self.engine.objects.get(id).await?;
            if let Some(trace) = &self.engine.trace {
                trace.object_get(object.kind as u8, object.canonical_bytes.len());
            }
            return Ok(object);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested object digest before returning it.
        let kind = ObjectKind::from_code(stored_kind).ok_or_else(|| {
            ObjectStoreError::Backend("packed metadata has an unknown object kind".into())
        })?;
        if let Some(trace) = &self.engine.trace {
            trace.object_get(kind as u8, bytes.len());
        }
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes.into(),
        })
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            result.push(match self.get(*id).await {
                Ok(_) => true,
                Err(ObjectStoreError::Missing(_)) => false,
                Err(e) => return Err(e),
            });
        }
        Ok(result)
    }
}

#[async_trait]
impl ContentBackend for PackedEngine {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        if let Some(trace) = &self.trace {
            trace.content_put_batch(
                contents.len(),
                contents.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut packed = Vec::new();
        for (id, bytes) in contents {
            let actual = ContentId::of(bytes);
            if actual != *id {
                return Err(ContentStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if bytes.len() > self.loose_threshold {
                self.content.put_many([(*id, bytes.clone())], 1)?;
            } else {
                packed.push((*id, bytes));
            }
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, bytes)| SegmentRecord {
                    namespace: CONTENT_NAMESPACE,
                    kind: 0,
                    digest: *id.as_bytes(),
                    bytes,
                })
                .collect::<Vec<_>>();
            self.catalog
                .lock()
                .map_err(|_| {
                    ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned"))
                })?
                .put_records(&records)
                .map_err(content_catalog_error)?;
        }
        Ok(())
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get_bounded(CONTENT_NAMESPACE, *id.as_bytes(), max)
            .map_err(|error| match error {
                CatalogError::Segment(super::segment::SegmentError::ReadLimit { minimum }) => {
                    ContentStoreError::TooLarge { id, max, minimum }
                }
                error => content_catalog_error(error),
            })?;
        if let Some((_, bytes)) = value.as_ref().filter(|(_, bytes)| bytes.len() > max) {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: bytes.len(),
            });
        }
        let Some((_kind, bytes)) = value else {
            // Oversized content bypasses packed segments. As with metadata,
            // avoid a failed loose-file probe for every ordinary packed read.
            let bytes = self.content.get_bounded(id, max)?;
            if let Some(trace) = &self.trace {
                trace.content_get(bytes.len());
            }
            return Ok(bytes);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested content digest before returning it.
        if let Some(trace) = &self.trace {
            trace.content_get(bytes.len());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traced_deferred_store_counts_storage_work_without_extending_its_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let (store, trace) = PackedObjectStore::open_deferred_traced(root.path()).unwrap();
        let bytes = wheat_core::DirectoryNode::default().canonical_bytes();
        let id = ObjectId::of_canonical_bytes(&bytes);
        store.put_if_absent(id, &bytes).await.unwrap();
        store.get(id).await.unwrap();

        let snapshot = trace.snapshot().unwrap();
        assert_eq!(snapshot.object_put_attempts, 1);
        assert_eq!(snapshot.object_gets, 1);
        assert!(snapshot.catalog_gets >= 2);
        assert_eq!(snapshot.deferred_appends, 1);
        assert_eq!(snapshot.deferred_index_transactions, 1);
        assert_eq!(snapshot.locator_recounts, 0);

        let final_snapshot = store.close_with_trace().unwrap().unwrap();
        assert_eq!(final_snapshot.object_put_attempts, 1);
        assert_eq!(final_snapshot.bulk_finalizations, 1);
        assert_eq!(final_snapshot.locator_recounts, 0);
        assert!(trace.snapshot().is_none());
    }

    #[test]
    fn deferred_close_failure_is_returned_by_packed_store() {
        let root = tempfile::tempdir().unwrap();
        let store = PackedObjectStore::open_deferred(root.path()).unwrap();
        store.test_fail_close_before_finalize().unwrap();
        let error = store.close().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("test failure before deferred bulk finalization")
        );
        PackedObjectStore::open_deferred(root.path())
            .unwrap()
            .close()
            .unwrap();
    }
}
