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
    segment::{
        CONTENT_NAMESPACE, MAX_SEGMENT_ENTRIES, MAX_SEGMENT_PAYLOAD_BYTES, METADATA_NAMESPACE,
        SegmentRecord,
    },
};
use crate::content_store::ContentBackend;
use crate::{
    ContentStore, ContentStoreError, ImmutableObjectStore, LooseObjectStore, ObjectKind,
    ObjectStoreError, StoredObject,
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
        Self::open_with_loose_threshold(root, DEFAULT_LOOSE_THRESHOLD)
    }

    pub fn open_with_loose_threshold(
        root: impl AsRef<Path>,
        loose_threshold: usize,
    ) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        let catalog = PackedCatalog::open(root.join("packed")).map_err(object_catalog_error)?;
        Ok(Self {
            engine: Arc::new(PackedEngine {
                catalog: Arc::new(Mutex::new(catalog)),
                objects: LooseObjectStore::new(root.join("loose-objects")),
                content: ContentStore::new(root.join("loose-content")),
                content_root: root.join("loose-content"),
                loose_threshold,
            }),
        })
    }

    #[must_use]
    pub fn content_store(&self) -> ContentStore {
        ContentStore::with_backend(self.engine.content_root.clone(), self.engine.clone())
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
        let mut start = 0;
        while start < packed.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < packed.len() && end - start < MAX_SEGMENT_ENTRIES {
                let next = packed[end].2.len();
                if end > start && bytes.saturating_add(next) > MAX_SEGMENT_PAYLOAD_BYTES as usize {
                    break;
                }
                bytes += next;
                end += 1;
            }
            let records = packed[start..end]
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
            start = end;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        match self.engine.objects.get(id).await {
            Ok(value) => return Ok(value),
            Err(ObjectStoreError::Missing(_)) => {}
            Err(error) => return Err(error),
        }
        let Some((stored_kind, bytes)) = self
            .catalog()?
            .get(METADATA_NAMESPACE, *id.as_bytes())
            .map_err(object_catalog_error)?
        else {
            return Err(ObjectStoreError::Missing(id));
        };
        let actual = ObjectId::of_canonical_bytes(&bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        let kind: ObjectKind = validate_canonical_object(&bytes)?.into();
        if stored_kind != kind as u16 {
            return Err(ObjectStoreError::Backend(
                "packed metadata kind does not match canonical bytes".into(),
            ));
        }
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes,
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

impl ContentBackend for PackedEngine {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
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
        let mut start = 0;
        while start < packed.len() {
            let mut end = start;
            let mut size = 0usize;
            while end < packed.len() && end - start < MAX_SEGMENT_ENTRIES {
                let next = packed[end].1.len();
                if end > start && size.saturating_add(next) > MAX_SEGMENT_PAYLOAD_BYTES as usize {
                    break;
                }
                size += next;
                end += 1;
            }
            let records = packed[start..end]
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
            start = end;
        }
        Ok(())
    }

    fn get(&self, id: ContentId) -> Result<Vec<u8>, ContentStoreError> {
        match self.content.get(id) {
            Ok(bytes) => return Ok(bytes),
            Err(ContentStoreError::Missing(_)) => {}
            Err(error) => return Err(error),
        }
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get(CONTENT_NAMESPACE, *id.as_bytes())
            .map_err(content_catalog_error)?;
        let Some((_kind, bytes)) = value else {
            return Err(ContentStoreError::Missing(id));
        };
        let actual = ContentId::of(&bytes);
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        Ok(bytes)
    }
}
