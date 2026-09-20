//! `RemotePackedStore` — an `ImmutableObjectStore` backed by S3-stored packed
//! segments with an in-memory index.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use object_store::{
    ObjectStore as BackendObjectStore, ObjectStoreExt,
    aws::{AmazonS3Builder, S3ConditionalPut},
};
use wheat_core::{ObjectId, SegmentId, validate_canonical_object};

use super::catalog::RemoteCatalog;
use super::content::RemoteContentBackend;
use super::segment_cache::SegmentCache;
use crate::packed::segment::{
    METADATA_NAMESPACE, SegmentBuilder, SegmentRecord,
};
use crate::{
    ContentStore, Durability, ImmutableObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};
use crate::object_store_adapter::S3ObjectStoreConfig;

const DEFAULT_SEGMENT_CACHE_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// Configuration for a remote packed object store.
#[derive(Clone, Debug)]
pub struct RemotePackedStoreConfig {
    pub s3: S3ObjectStoreConfig,
    /// Maximum bytes of cached segment data (default: 256 MiB).
    pub segment_cache_bytes: Option<usize>,
}

/// An object buffered for the next seal.
struct PendingObject {
    id: ObjectId,
    kind: ObjectKind,
    canonical_bytes: Vec<u8>,
}

struct StoreState {
    catalog: RemoteCatalog,
    cache: SegmentCache,
    pending: Vec<PendingObject>,
    /// Dedup set for pending objects.
    pending_ids: std::collections::HashSet<ObjectId>,
}

/// S3-backed packed object store.
///
/// Structural objects are buffered and sealed into packed segments on `seal()`.
/// The in-memory index is rebuilt from the S3 catalog chain on startup.
pub struct RemotePackedStore {
    backend: Arc<dyn BackendObjectStore>,
    prefix: String,
    state: Mutex<StoreState>,
    content_prefix: String,
}

impl std::fmt::Debug for RemotePackedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemotePackedStore")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RemotePackedStore {
    /// Open a remote packed store against the configured S3 endpoint.
    ///
    /// Downloads and authenticates the catalog chain to build the in-memory
    /// index. Returns an empty store if no catalog exists yet.
    pub async fn open(config: &RemotePackedStoreConfig) -> Result<Self, ObjectStoreError> {
        let s3 = &config.s3;
        let backend: Arc<dyn BackendObjectStore> = Arc::new(
            AmazonS3Builder::new()
                .with_endpoint(&s3.endpoint)
                .with_bucket_name(&s3.bucket)
                .with_region(&s3.region)
                .with_access_key_id(&s3.access_key_id)
                .with_secret_access_key(&s3.secret_access_key)
                .with_allow_http(s3.allow_http)
                .with_virtual_hosted_style_request(false)
                .with_conditional_put(S3ConditionalPut::ETagMatch)
                .build()
                .map_err(|e| ObjectStoreError::Backend(e.to_string()))?,
        );

        let prefix = s3.prefix.trim_matches('/').to_owned();
        let catalog = RemoteCatalog::load(&backend, &prefix)
            .await
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;

        let cache_bytes = config
            .segment_cache_bytes
            .unwrap_or(DEFAULT_SEGMENT_CACHE_BYTES);

        let content_prefix = prefix.clone();

        Ok(Self {
            backend,
            prefix,
            state: Mutex::new(StoreState {
                catalog,
                cache: SegmentCache::new(cache_bytes),
                pending: Vec::new(),
                pending_ids: std::collections::HashSet::new(),
            }),
            content_prefix,
        })
    }

    /// Returns a `ContentStore` backed by S3 individual keys.
    pub fn content_store(&self) -> ContentStore {
        let backend = RemoteContentBackend::new(
            Arc::clone(&self.backend),
            self.content_prefix.clone(),
        );
        // Use a dummy root — the RemoteContentBackend handles all I/O.
        // The ContentStore's loose fallback never fires because the backend
        // is always present.
        ContentStore::with_backend(
            std::path::PathBuf::from("/dev/null"),
            Arc::new(backend),
            Durability::Durable,
        )
    }

    /// Download a segment into the cache if not already present, then look up
    /// the object.
    fn fetch_from_segment(
        &self,
        state: &mut StoreState,
        segment_id: SegmentId,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, ObjectStoreError> {
        // Check cache first.
        if let Some(result) = state
            .cache
            .get(segment_id, namespace, digest)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
        {
            return Ok(Some(result));
        }

        // Download the segment from S3.
        let seg_path = format!(
            "{}/segments/{}.segment",
            self.prefix,
            hex::encode(segment_id.0)
        );
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let seg_bytes = tokio::task::block_in_place(|| {
            rt.block_on(async {
                let path = object_store::path::Path::from(seg_path);
                self.backend
                    .get(&path)
                    .await
                    .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
                    .bytes()
                    .await
                    .map_err(|e| ObjectStoreError::Backend(e.to_string()))
            })
        })?;

        // Insert into cache (authenticates the segment).
        let reader = state
            .cache
            .insert(segment_id, &seg_bytes)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;

        // Look up the object in the now-cached segment.
        reader
            .lookup_entry_bounded(namespace, digest, usize::MAX)
            .map(|opt| opt.map(|(entry, bytes)| (entry.kind, bytes)))
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))
    }
}

#[async_trait]
impl ImmutableObjectStore for RemotePackedStore {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        let kind: ObjectKind = validate_canonical_object(canonical_bytes)?.into();
        let actual = ObjectId::of_canonical_bytes(canonical_bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;

        // Skip if already known (in catalog or pending).
        if state.catalog.index.contains_key(&(METADATA_NAMESPACE, *id.as_bytes())) {
            return Ok(());
        }
        if state.pending_ids.contains(&id) {
            return Ok(());
        }

        state.pending.push(PendingObject {
            id,
            kind,
            canonical_bytes: canonical_bytes.to_vec(),
        });
        state.pending_ids.insert(id);
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;

        // Check pending buffer first.
        for pending in &state.pending {
            if pending.id == id {
                return Ok(StoredObject {
                    id,
                    kind: pending.kind,
                    canonical_bytes: pending.canonical_bytes.clone().into(),
                });
            }
        }

        // Look up in the catalog index.
        let digest = *id.as_bytes();
        let Some(locator) = state.catalog.index.get(&(METADATA_NAMESPACE, digest)).cloned() else {
            return Err(ObjectStoreError::Missing(id));
        };

        let (kind_code, bytes) = self
            .fetch_from_segment(&mut state, locator.segment_id, METADATA_NAMESPACE, digest)?
            .ok_or(ObjectStoreError::Missing(id))?;

        let kind = ObjectKind::from_code(kind_code).ok_or_else(|| {
            ObjectStoreError::Backend("unknown object kind in segment".into())
        })?;

        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes.into(),
        })
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;

        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            let digest = *id.as_bytes();
            let in_catalog = state
                .catalog
                .index
                .contains_key(&(METADATA_NAMESPACE, digest));
            let in_pending = state.pending_ids.contains(id);
            results.push(in_catalog || in_pending);
        }
        Ok(results)
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        let pending = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;
            if state.pending.is_empty() {
                return Ok(());
            }
            let pending = std::mem::take(&mut state.pending);
            state.pending_ids.clear();
            pending
        };

        // Build a segment from the pending objects.
        let tmp_dir = std::env::temp_dir();
        let mut builder = SegmentBuilder::new(&tmp_dir)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;

        for obj in &pending {
            let record = SegmentRecord {
                namespace: METADATA_NAMESPACE,
                kind: obj.kind as u16,
                digest: *obj.id.as_bytes(),
                bytes: &obj.canonical_bytes,
            };
            builder
                .push(record)
                .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        }

        let sealed = builder
            .seal()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;

        // Read the segment bytes for upload.
        let segment_bytes = {
            use std::io::Read;
            let mut file = sealed.file.reopen()
                .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
            let mut buf = Vec::with_capacity(sealed.metadata.total_length as usize);
            file.read_to_end(&mut buf)
                .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
            buf
        };

        // Re-open to extract the authenticated entries.
        let reader_file = sealed.file.reopen()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let reader = crate::packed::segment::SegmentReader::open(reader_file)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let entries: Vec<_> = reader.authenticated_entries().to_vec();

        // Upload segment and update catalog.
        let mut state = self
            .state
            .lock()
            .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;

        // Use block_in_place to call async catalog methods from sync context.
        let backend = Arc::clone(&self.backend);
        let prefix = self.prefix.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                state
                    .catalog
                    .seal_segment(&backend, &prefix, sealed.id, &entries, &segment_bytes)
                    .await
                    .map_err(|e| ObjectStoreError::Backend(e.to_string()))
            })
        })?;

        // Also insert the segment into the cache so subsequent reads are fast.
        state
            .cache
            .insert(sealed.id, &segment_bytes)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;

        Ok(())
    }
}
