use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use thiserror::Error;
use wheat_core::{CanonicalObjectError, CanonicalObjectKind, ObjectId, validate_canonical_object};

use crate::{Durability, codec};

const MAX_BATCH_WORKERS: usize = 16;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ObjectKind {
    Tree = 1,
    Change = 2,
    Revision = 3,
    Application = 4,
    Conflict = 5,
    View = 6,
    ContentMapNode = 7,
    RepositoryState = 8,
    RevisionMapNode = 9,
    ApplicationMapNode = 10,
    ConflictSetNode = 11,
    ContentSetNode = 12,
    RetainedSetNode = 13,
    ViewLabelManifest = 14,
    RepositoryEnvelope = 15,
    Edit = 16,
    ChangeMapNode = 17,
    ViewLineageMapNode = 18,
}

impl ObjectKind {
    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => Self::Tree,
            2 => Self::Change,
            3 => Self::Revision,
            4 => Self::Application,
            5 => Self::Conflict,
            6 => Self::View,
            7 => Self::ContentMapNode,
            8 => Self::RepositoryState,
            9 => Self::RevisionMapNode,
            10 => Self::ApplicationMapNode,
            11 => Self::ConflictSetNode,
            12 => Self::ContentSetNode,
            13 => Self::RetainedSetNode,
            14 => Self::ViewLabelManifest,
            15 => Self::RepositoryEnvelope,
            16 => Self::Edit,
            17 => Self::ChangeMapNode,
            18 => Self::ViewLineageMapNode,
            _ => return None,
        })
    }
}

impl From<CanonicalObjectKind> for ObjectKind {
    fn from(value: CanonicalObjectKind) -> Self {
        match value {
            CanonicalObjectKind::Tree => Self::Tree,
            CanonicalObjectKind::Change => Self::Change,
            CanonicalObjectKind::Revision => Self::Revision,
            CanonicalObjectKind::Application => Self::Application,
            CanonicalObjectKind::Conflict => Self::Conflict,
            CanonicalObjectKind::View => Self::View,
            CanonicalObjectKind::ContentMapNode => Self::ContentMapNode,
            CanonicalObjectKind::RepositoryState => Self::RepositoryState,
            CanonicalObjectKind::RevisionMapNode => Self::RevisionMapNode,
            CanonicalObjectKind::ApplicationMapNode => Self::ApplicationMapNode,
            CanonicalObjectKind::ConflictSetNode => Self::ConflictSetNode,
            CanonicalObjectKind::ContentSetNode => Self::ContentSetNode,
            CanonicalObjectKind::RetainedSetNode => Self::RetainedSetNode,
            CanonicalObjectKind::ViewLabelManifest => Self::ViewLabelManifest,
            CanonicalObjectKind::RepositoryEnvelope => Self::RepositoryEnvelope,
            CanonicalObjectKind::Edit => Self::Edit,
            CanonicalObjectKind::ChangeMapNode => Self::ChangeMapNode,
            CanonicalObjectKind::ViewLineageMapNode => Self::ViewLineageMapNode,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    pub id: ObjectId,
    pub kind: ObjectKind,
    /// Shared immutable storage keeps cache hits allocation-free.
    pub canonical_bytes: Bytes,
}

#[derive(Debug, Error)]
pub enum ObjectStoreError {
    #[error("object {0} is missing")]
    Missing(ObjectId),
    #[error("object {id} has kind {actual:?}, expected {expected:?}")]
    WrongKind {
        id: ObjectId,
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("object {expected} failed integrity verification (found {actual})")]
    Corrupt {
        expected: ObjectId,
        actual: ObjectId,
    },
    #[error("invalid Wheat object: {0}")]
    InvalidFormat(#[from] CanonicalObjectError),
    #[error("object {id} has an invalid physical envelope: {reason}")]
    InvalidEnvelope { id: ObjectId, reason: String },
    #[error("object {id} changed while it was being read")]
    Changed { id: ObjectId },
    #[error("immutable object storage integrity failure: {0}")]
    Integrity(String),
    #[error("immutable object backend failed: {0}")]
    Backend(String),
    #[error("immutable object store I/O failed")]
    Io(#[from] io::Error),
}

#[async_trait]
pub trait ImmutableObjectStore: Send + Sync {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError>;

    /// Durably stores a batch. Implementations may override this to parallelize
    /// backend work; successful completion has the same durability guarantee as
    /// awaiting every corresponding [`Self::put_if_absent`] call.
    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        let concurrency = concurrency.clamp(1, MAX_BATCH_WORKERS);
        let results = stream::iter(
            objects
                .iter()
                .cloned()
                .map(|(id, bytes)| async move { self.put_if_absent(id, &bytes).await }),
        )
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        results.into_iter().collect()
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError>;

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError>;

    /// Finalize any pending writes into a durable batch.
    ///
    /// Stores that buffer objects (e.g. into packed segments for upload to a
    /// remote object store) override this to seal and persist the buffer.
    /// The default implementation is a no-op.
    async fn seal(&self) -> Result<(), ObjectStoreError> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableObjectStore for Box<dyn ImmutableObjectStore> {
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

#[derive(Clone, Debug)]
pub struct LooseObjectStore {
    root: PathBuf,
    durability: Durability,
}

impl LooseObjectStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_durability(root, Durability::Durable)
    }

    #[must_use]
    pub fn with_durability(root: impl Into<PathBuf>, durability: Durability) -> Self {
        Self {
            root: root.into(),
            durability,
        }
    }

    pub async fn put(&self, canonical_bytes: &[u8]) -> Result<ObjectId, ObjectStoreError> {
        validate_canonical_object(canonical_bytes)?;
        let id = ObjectId::of_canonical_bytes(canonical_bytes);
        self.put_if_absent(id, canonical_bytes).await?;
        Ok(id)
    }

    pub async fn get_kind(
        &self,
        id: ObjectId,
        expected: ObjectKind,
    ) -> Result<StoredObject, ObjectStoreError> {
        let object = self.get(id).await?;
        if object.kind != expected {
            return Err(ObjectStoreError::WrongKind {
                id,
                expected,
                actual: object.kind,
            });
        }
        Ok(object)
    }

    #[must_use]
    pub fn path(&self, id: ObjectId) -> PathBuf {
        let encoded = id.to_string();
        self.root.join(&encoded[..2]).join(&encoded[2..])
    }

    /// Synchronous verified read, for callers already holding a store lock.
    pub(crate) fn read_verified(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        Self::verify_bytes(id, self.read_existing(id)?)
    }

    /// IDs of every canonically named object file, sorted. Temporary files
    /// (dot-prefixed) and foreign names are ignored.
    pub(crate) fn list_ids(&self) -> Result<Vec<ObjectId>, ObjectStoreError> {
        let mut ids = Vec::new();
        let shards = match fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(error.into()),
        };
        for shard in shards {
            let shard = shard?;
            let Some(prefix) = shard.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if prefix.len() != 2 || !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let Some(rest) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !entry.file_type()?.is_file() {
                    continue;
                }
                if let Ok(id) = format!("{prefix}{rest}").parse::<ObjectId>()
                    && id.to_string() == format!("{prefix}{rest}")
                {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Removes the loose files of `ids` (missing files are fine), syncing each
    /// changed shard directory once. Callers must already hold another durable
    /// copy of every object.
    pub(crate) fn retire(&self, ids: &[ObjectId]) -> Result<(), ObjectStoreError> {
        let mut changed = std::collections::BTreeSet::new();
        for id in ids {
            let path = self.path(*id);
            match fs::remove_file(&path) {
                Ok(()) => {
                    if let Some(parent) = path.parent() {
                        changed.insert(parent.to_path_buf());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for directory in changed {
            self.durability.sync_directory(&directory)?;
        }
        Ok(())
    }

    fn verify_bytes(
        id: ObjectId,
        canonical_bytes: Vec<u8>,
    ) -> Result<StoredObject, ObjectStoreError> {
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

    fn read_existing(&self, id: ObjectId) -> Result<Vec<u8>, ObjectStoreError> {
        let mut file = match fs::File::open(self.path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ObjectStoreError::Missing(id));
            }
            Err(error) => return Err(ObjectStoreError::Io(error)),
        };
        let physical_length = file.metadata()?.len();
        codec::read_envelope_file(&mut file, physical_length, codec::HARD_LOGICAL_RECORD_BYTES)
            .map_err(|error| match error {
                codec::EnvelopeFileError::Codec(error) => ObjectStoreError::InvalidEnvelope {
                    id,
                    reason: error.to_string(),
                },
                codec::EnvelopeFileError::Changed => ObjectStoreError::Changed { id },
                codec::EnvelopeFileError::Io(error) => ObjectStoreError::Io(error),
            })
    }

    fn verify_existing(&self, id: ObjectId, expected: &[u8]) -> Result<(), ObjectStoreError> {
        let stored = Self::verify_bytes(id, self.read_existing(id)?)?;
        if stored.canonical_bytes != expected {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual: ObjectId::of_canonical_bytes(&stored.canonical_bytes),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableObjectStore for LooseObjectStore {
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

        let path = self.path(id);
        let parent = path.parent().expect("object path has parent");
        let root_was_missing = !self.root.exists();
        let parent_was_missing = !parent.exists();
        fs::create_dir_all(parent)?;
        if root_was_missing && let Some(root_parent) = self.root.parent() {
            self.durability.sync_directory(root_parent)?;
        }
        if parent_was_missing {
            self.durability.sync_directory(&self.root)?;
        }
        let temporary = temporary_path(parent, id);
        let result = (|| {
            let encoded = codec::encode_envelope(canonical_bytes)
                .map_err(|error| ObjectStoreError::Backend(error.to_string()))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&encoded)?;
            self.durability.sync_file(&file)?;

            match fs::hard_link(&temporary, &path) {
                Ok(()) => {
                    fs::remove_file(&temporary)?;
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match self.verify_existing(id, canonical_bytes) {
                        Ok(()) => {
                            fs::remove_file(&temporary)?;
                            Ok(())
                        }
                        Err(ObjectStoreError::Missing(_)) => {
                            fs::hard_link(&temporary, &path)?;
                            fs::remove_file(&temporary)?;
                            self.durability.sync_directory(parent)?;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(ObjectStoreError::Io(error)),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        // Loose writes perform blocking filesystem I/O; polling async futures on
        // one thread cannot overlap their fsync waits. Borrow bounded batches
        // into scoped workers and join every worker before acknowledging flush.
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles = (0..concurrency.clamp(1, MAX_BATCH_WORKERS).min(objects.len()))
                .map(|_| {
                    scope.spawn(|| {
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some((id, bytes)) = objects.get(index) else {
                                return Ok(());
                            };
                            futures::executor::block_on(self.put_if_absent(*id, bytes))?;
                        }
                    })
                })
                .collect::<Vec<_>>();
            let mut result = Ok(());
            for handle in handles {
                let outcome = handle.join().unwrap_or_else(|_| {
                    Err(ObjectStoreError::Backend(
                        "loose batch worker panicked".into(),
                    ))
                });
                if result.is_ok() {
                    result = outcome;
                }
            }
            result
        })
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        Self::verify_bytes(id, self.read_existing(id)?)
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            results.push(match self.get(*id).await {
                Ok(_) => true,
                Err(ObjectStoreError::Missing(_)) => false,
                Err(error) => return Err(error),
            });
        }
        Ok(results)
    }
}

fn temporary_path(parent: &Path, id: ObjectId) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{id}.{}.{}.tmp", std::process::id(), sequence))
}
