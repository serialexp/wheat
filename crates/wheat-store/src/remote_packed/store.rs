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
use crate::object_store_adapter::S3ObjectStoreConfig;
use crate::packed::segment::{
    MAX_SEGMENT_ENTRIES, MAX_SEGMENT_PAYLOAD_BYTES, METADATA_NAMESPACE, PreparedSegmentRecord,
    SegmentBuilder, SegmentError, SegmentRecord, prepare_record,
};
use crate::{
    ContentStore, Durability, ImmutableObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};

const DEFAULT_SEGMENT_CACHE_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

fn segment_integrity(error: SegmentError) -> ObjectStoreError {
    match error {
        SegmentError::Io(error) => ObjectStoreError::Io(error),
        other => ObjectStoreError::Integrity(other.to_string()),
    }
}

fn catalog_error(error: super::catalog::CatalogError) -> ObjectStoreError {
    match error {
        super::catalog::CatalogError::Backend(error) => {
            ObjectStoreError::Backend(error.to_string())
        }
        super::catalog::CatalogError::Io(error) => ObjectStoreError::Io(error),
        super::catalog::CatalogError::Segment(error) => segment_integrity(error),
        super::catalog::CatalogError::Corrupt(reason) => ObjectStoreError::Integrity(reason),
        super::catalog::CatalogError::HeadFormat(error) => {
            ObjectStoreError::Integrity(error.to_string())
        }
    }
}

/// Configuration for a remote packed object store.
#[derive(Clone, Debug)]
pub struct RemotePackedStoreConfig {
    pub s3: S3ObjectStoreConfig,
    /// Maximum bytes of cached segment data (default: 256 MiB).
    pub segment_cache_bytes: Option<usize>,
}

/// An object buffered for the next seal.
#[derive(Clone)]
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
    #[cfg(test)]
    segment_limits: TestSegmentLimits,
    #[cfg(test)]
    fail_next_publication: bool,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestSegmentLimits {
    max_entries: usize,
    max_payload_bytes: u64,
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

        Self::open_backend(
            backend,
            s3.prefix.trim_matches('/').to_owned(),
            config
                .segment_cache_bytes
                .unwrap_or(DEFAULT_SEGMENT_CACHE_BYTES),
        )
        .await
    }

    async fn open_backend(
        backend: Arc<dyn BackendObjectStore>,
        prefix: String,
        cache_bytes: usize,
    ) -> Result<Self, ObjectStoreError> {
        let mut catalog = RemoteCatalog::load(&backend, &prefix)
            .await
            .map_err(catalog_error)?;

        // Pre-warm the segment cache with segments already downloaded
        // during catalog load, avoiding redundant S3 GETs on first access.
        let preloaded = std::mem::take(&mut catalog.preloaded_segments);
        let mut cache = SegmentCache::new(cache_bytes);
        for (segment_id, segment_bytes) in &preloaded {
            cache
                .insert(*segment_id, segment_bytes)
                .map_err(segment_integrity)?;
        }

        let content_prefix = prefix.clone();

        Ok(Self {
            backend,
            prefix,
            state: Mutex::new(StoreState {
                catalog,
                cache,
                pending: Vec::new(),
                pending_ids: std::collections::HashSet::new(),
                #[cfg(test)]
                segment_limits: TestSegmentLimits {
                    max_entries: MAX_SEGMENT_ENTRIES,
                    max_payload_bytes: MAX_SEGMENT_PAYLOAD_BYTES,
                },
                #[cfg(test)]
                fail_next_publication: false,
            }),
            content_prefix,
        })
    }

    #[cfg(test)]
    async fn in_memory(segment_limits: TestSegmentLimits) -> Self {
        let backend: Arc<dyn BackendObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = Self::open_backend(
            backend,
            "test/remote-packed".to_owned(),
            DEFAULT_SEGMENT_CACHE_BYTES,
        )
        .await
        .expect("open in-memory remote packed store");
        store.state.lock().expect("store state lock").segment_limits = segment_limits;
        store
    }

    /// Returns a `ContentStore` backed by S3 individual keys.
    pub fn content_store(&self) -> ContentStore {
        let backend =
            RemoteContentBackend::new(Arc::clone(&self.backend), self.content_prefix.clone());
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
            .map_err(segment_integrity)?
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
            .map_err(segment_integrity)?;

        // Look up the object in the now-cached segment.
        reader
            .lookup_entry_bounded(namespace, digest, usize::MAX)
            .map(|opt| opt.map(|(entry, bytes)| (entry.kind, bytes)))
            .map_err(segment_integrity)
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
        if state
            .catalog
            .index
            .contains_key(&(METADATA_NAMESPACE, *id.as_bytes()))
        {
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
        let Some(locator) = state
            .catalog
            .index
            .get(&(METADATA_NAMESPACE, digest))
            .cloned()
        else {
            return Err(ObjectStoreError::Missing(id));
        };

        let (kind_code, bytes) = self
            .fetch_from_segment(&mut state, locator.segment_id, METADATA_NAMESPACE, digest)?
            .ok_or(ObjectStoreError::Missing(id))?;

        let kind = ObjectKind::from_code(kind_code)
            .ok_or_else(|| ObjectStoreError::Integrity("unknown object kind in segment".into()))?;

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
        let mut state = self
            .state
            .lock()
            .map_err(|_| ObjectStoreError::Backend("mutex poisoned".into()))?;
        while !state.pending.is_empty() {
            // Clone only the current immutable logical batch. The authoritative
            // pending queue remains intact until its catalog head is published,
            // so every failure leaves these objects readable and retryable.
            let pending = state.pending.clone();
            #[cfg(not(test))]
            let (max_entries, max_payload_bytes) = (MAX_SEGMENT_ENTRIES, MAX_SEGMENT_PAYLOAD_BYTES);
            #[cfg(test)]
            let (max_entries, max_payload_bytes) = (
                state.segment_limits.max_entries,
                state.segment_limits.max_payload_bytes,
            );
            let mut prepared = Vec::<PreparedSegmentRecord<'_>>::new();
            let mut stored_payload = 0_u64;
            for object in &pending {
                let record = SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind: object.kind as u16,
                    digest: *object.id.as_bytes(),
                    bytes: &object.canonical_bytes,
                };
                let record = prepare_record(record).map_err(segment_integrity)?;
                let next_payload = stored_payload
                    .checked_add(record.stored_length())
                    .ok_or_else(|| segment_integrity(SegmentError::TooLarge))?;
                if !prepared.is_empty()
                    && (prepared.len() == max_entries || next_payload > max_payload_bytes)
                {
                    break;
                }
                if next_payload > max_payload_bytes {
                    return Err(segment_integrity(SegmentError::TooLarge));
                }
                stored_payload = next_payload;
                prepared.push(record);
            }

            let batch_len = prepared.len();
            let mut builder = SegmentBuilder::new(std::env::temp_dir())
                .map_err(|error| ObjectStoreError::Io(std::io::Error::other(error)))?;
            for record in prepared {
                builder.push_prepared(record).map_err(segment_integrity)?;
            }
            let sealed = builder.seal().map_err(segment_integrity)?;

            use std::io::Read;
            let mut file = sealed.file.reopen()?;
            let capacity = usize::try_from(sealed.metadata.total_length)
                .map_err(|_| segment_integrity(SegmentError::TooLarge))?;
            let mut segment_bytes = Vec::with_capacity(capacity);
            file.read_to_end(&mut segment_bytes)?;
            let reader = crate::packed::segment::SegmentReader::open(sealed.file.reopen()?)
                .map_err(segment_integrity)?;
            let entries = reader.authenticated_entries().to_vec();

            // Authenticate/cache before publishing. A failed catalog operation
            // leaves pending untouched; retrying may overwrite the same
            // content-addressed segment and catalog record safely.
            state
                .cache
                .insert(sealed.id, &segment_bytes)
                .map_err(segment_integrity)?;
            #[cfg(test)]
            if std::mem::take(&mut state.fail_next_publication) {
                return Err(ObjectStoreError::Backend(
                    "injected catalog publication failure".into(),
                ));
            }
            let backend = Arc::clone(&self.backend);
            let prefix = self.prefix.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    state
                        .catalog
                        .seal_segment(&backend, &prefix, sealed.id, &entries, &segment_bytes)
                        .await
                        .map_err(catalog_error)
                })
            })?;

            let published: Vec<_> = state.pending.drain(..batch_len).collect();
            for object in published {
                state.pending_ids.remove(&object.id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use wheat_core::{DirectoryEntry, DirectoryNode, TreeId};

    use super::*;

    fn object(discriminator: u8) -> (ObjectId, Vec<u8>) {
        let mut node = DirectoryNode::default();
        node.entries.insert(
            format!("child-{discriminator}"),
            DirectoryEntry::Directory(TreeId::from_digest([discriminator; 32])),
        );
        let bytes = node.canonical_bytes();
        (ObjectId::of_canonical_bytes(&bytes), bytes)
    }

    async fn put_objects(store: &RemotePackedStore, count: u8) -> Vec<(ObjectId, Vec<u8>)> {
        let objects: Vec<_> = (0..count).map(object).collect();
        for (id, bytes) in &objects {
            store
                .put_if_absent(*id, bytes)
                .await
                .expect("buffer object");
        }
        objects
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_publication_keeps_pending_objects_readable_and_retryable() {
        let store = RemotePackedStore::in_memory(TestSegmentLimits {
            max_entries: MAX_SEGMENT_ENTRIES,
            max_payload_bytes: MAX_SEGMENT_PAYLOAD_BYTES,
        })
        .await;
        let objects = put_objects(&store, 3).await;
        store
            .state
            .lock()
            .expect("store state lock")
            .fail_next_publication = true;

        let error = store.seal().await.expect_err("publication must fail");
        assert!(
            error
                .to_string()
                .contains("injected catalog publication failure")
        );
        assert_eq!(
            store
                .exists_many(&objects.iter().map(|(id, _)| *id).collect::<Vec<_>>())
                .await
                .expect("check pending objects"),
            vec![true; objects.len()]
        );
        for (id, expected) in &objects {
            assert_eq!(
                &*store
                    .get(*id)
                    .await
                    .expect("read pending object")
                    .canonical_bytes,
                expected
            );
        }

        store.seal().await.expect("retry publication");
        for (id, expected) in &objects {
            assert_eq!(
                &*store
                    .get(*id)
                    .await
                    .expect("read sealed object")
                    .canonical_bytes,
                expected
            );
        }
        let state = store.state.lock().expect("store state lock");
        assert!(state.pending.is_empty());
        assert_eq!(state.catalog.segments.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_segment_limit_rolls_one_pending_batch_into_multiple_segments() {
        let store = RemotePackedStore::in_memory(TestSegmentLimits {
            max_entries: 2,
            max_payload_bytes: MAX_SEGMENT_PAYLOAD_BYTES,
        })
        .await;
        let objects = put_objects(&store, 5).await;

        store.seal().await.expect("seal all pending objects");

        for (id, expected) in &objects {
            assert_eq!(
                &*store
                    .get(*id)
                    .await
                    .expect("read sealed object")
                    .canonical_bytes,
                expected
            );
        }
        let state = store.state.lock().expect("store state lock");
        assert!(state.pending.is_empty());
        assert_eq!(state.catalog.segments.len(), 3);
        assert_eq!(state.catalog.index.len(), objects.len());
    }
}
