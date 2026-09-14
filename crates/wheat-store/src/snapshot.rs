//! Bounded, authenticated native repository snapshots.
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{Connection, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    ApplicationResultRecord, ContentId, DirectoryEntry, EditFailure, EditOperation, ObjectId,
    RepositoryEnvelopeId,
};

use crate::{
    ContentStore, ContentStoreError, ImmutableObjectStore, ObjectKind, ObjectStoreError,
    PublicationError, PublicationStore, RepositoryHead, RepositoryPublicationError,
    RepositoryReader, StoredObject,
};

const MAGIC: &[u8; 8] = b"WHEATSNP";
const VERSION: u16 = 1;
const INVENTORY_ENTRY_BYTES: usize = 40;
const MANIFEST_FIXED_BYTES: usize = 98;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotLimits {
    pub max_objects: usize,
    pub max_contents: usize,
    pub max_object_bytes: usize,
    pub max_content_bytes: usize,
    pub max_total_bytes: usize,
    pub max_manifest_bytes: usize,
    pub batch_items: usize,
    pub batch_bytes: usize,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_objects: 1_000_000,
            max_contents: 1_000_000,
            max_object_bytes: 16 * 1024 * 1024,
            max_content_bytes: 256 * 1024 * 1024,
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_manifest_bytes: 96 * 1024 * 1024,
            batch_items: 256,
            batch_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InventoryEntry<I> {
    pub id: I,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotManifest {
    pub head: RepositoryHead,
    pub envelope: RepositoryEnvelopeId,
    pub objects: Vec<InventoryEntry<ObjectId>>,
    pub contents: Vec<InventoryEntry<ContentId>>,
}

impl SnapshotManifest {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            MANIFEST_FIXED_BYTES + (self.objects.len() + self.contents.len()) * 40,
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_be_bytes());
        out.extend_from_slice(&self.head.generation.to_be_bytes());
        out.extend_from_slice(self.head.envelope.as_bytes());
        out.extend_from_slice(self.envelope.as_bytes());
        encode_inventory(&mut out, &self.objects, |id| id.as_bytes());
        encode_inventory(&mut out, &self.contents, |id| id.as_bytes());
        out
    }

    #[must_use]
    pub fn id(&self) -> SnapshotId {
        SnapshotId(*blake3::hash(&self.canonical_bytes()).as_bytes())
    }

    #[must_use]
    pub fn repository_ownership(&self, repository_id: [u8; 32]) -> crate::OwnershipManifest {
        crate::OwnershipManifest {
            owner: crate::Owner::repository(repository_id),
            objects: self.objects.clone(),
            contents: self.contents.clone(),
        }
    }

    #[must_use]
    pub fn snapshot_ownership(&self) -> crate::OwnershipManifest {
        crate::OwnershipManifest {
            owner: crate::Owner::snapshot(self.id()),
            objects: self.objects.clone(),
            contents: self.contents.clone(),
        }
    }

    /// Decodes canonical bytes after bounding the manifest and each allocation.
    pub fn decode(bytes: &[u8], limits: SnapshotLimits) -> Result<Self, SnapshotError> {
        validate_limits(limits)?;
        if bytes.len() > limits.max_manifest_bytes {
            return Err(SnapshotError::LimitExceeded("manifest bytes"));
        }
        let fixed = MAGIC.len() + 2 + 8 + 32 + 32;
        if bytes.len() < fixed || &bytes[..MAGIC.len()] != MAGIC {
            return Err(SnapshotError::InvalidManifest(
                "bad magic or truncated header",
            ));
        }
        let mut offset = MAGIC.len();
        if read_u16(bytes, &mut offset)? != VERSION {
            return Err(SnapshotError::InvalidManifest("unsupported version"));
        }
        let generation = read_u64(bytes, &mut offset)?;
        let head_envelope = RepositoryEnvelopeId::from_digest(read_digest(bytes, &mut offset)?);
        let envelope = RepositoryEnvelopeId::from_digest(read_digest(bytes, &mut offset)?);
        let objects = decode_inventory(
            bytes,
            &mut offset,
            limits.max_objects,
            ObjectId::from_digest,
        )?;
        let contents = decode_inventory(
            bytes,
            &mut offset,
            limits.max_contents,
            ContentId::from_digest,
        )?;
        if offset != bytes.len() {
            return Err(SnapshotError::InvalidManifest("trailing bytes"));
        }
        let manifest = Self {
            head: RepositoryHead {
                generation,
                envelope: head_envelope,
            },
            envelope,
            objects,
            contents,
        };
        manifest.validate(limits)?;
        Ok(manifest)
    }

    pub fn validate(&self, limits: SnapshotLimits) -> Result<(), SnapshotError> {
        validate_limits(limits)?;
        if self.head.envelope != self.envelope {
            return Err(SnapshotError::InvalidManifest("head and envelope differ"));
        }
        validate_inventory(&self.objects, limits.max_objects, limits.max_object_bytes)?;
        validate_inventory(
            &self.contents,
            limits.max_contents,
            limits.max_content_bytes,
        )?;
        let encoded = MANIFEST_FIXED_BYTES
            .checked_add(
                (self.objects.len() + self.contents.len())
                    .checked_mul(INVENTORY_ENTRY_BYTES)
                    .ok_or(SnapshotError::LimitExceeded("manifest bytes"))?,
            )
            .ok_or(SnapshotError::LimitExceeded("manifest bytes"))?;
        if encoded > limits.max_manifest_bytes {
            return Err(SnapshotError::LimitExceeded("manifest bytes"));
        }
        let object_bytes = self
            .objects
            .iter()
            .try_fold(0_u64, |sum, item| sum.checked_add(item.bytes))
            .ok_or(SnapshotError::LimitExceeded("total bytes"))?;
        let total = self
            .contents
            .iter()
            .try_fold(object_bytes, |sum, item| sum.checked_add(item.bytes))
            .ok_or(SnapshotError::LimitExceeded("total bytes"))?;
        if total > limits.max_total_bytes as u64 {
            return Err(SnapshotError::LimitExceeded("total bytes"));
        }
        Ok(())
    }
}

fn encode_inventory<I>(
    out: &mut Vec<u8>,
    inventory: &[InventoryEntry<I>],
    digest: impl Fn(&I) -> &[u8; 32],
) {
    out.extend_from_slice(&(inventory.len() as u64).to_be_bytes());
    for item in inventory {
        out.extend_from_slice(digest(&item.id));
        out.extend_from_slice(&item.bytes.to_be_bytes());
    }
}

fn decode_inventory<I>(
    bytes: &[u8],
    offset: &mut usize,
    max_count: usize,
    id: impl Fn([u8; 32]) -> I,
) -> Result<Vec<InventoryEntry<I>>, SnapshotError> {
    let count = usize::try_from(read_u64(bytes, offset)?)
        .map_err(|_| SnapshotError::LimitExceeded("inventory count"))?;
    if count > max_count {
        return Err(SnapshotError::LimitExceeded("inventory count"));
    }
    let encoded = count
        .checked_mul(INVENTORY_ENTRY_BYTES)
        .ok_or(SnapshotError::LimitExceeded("inventory bytes"))?;
    if bytes.len().saturating_sub(*offset) < encoded {
        return Err(SnapshotError::InvalidManifest("truncated inventory"));
    }
    let mut inventory = Vec::with_capacity(count);
    for _ in 0..count {
        inventory.push(InventoryEntry {
            id: id(read_digest(bytes, offset)?),
            bytes: read_u64(bytes, offset)?,
        });
    }
    Ok(inventory)
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, SnapshotError> {
    let value = bytes
        .get(*offset..*offset + 2)
        .ok_or(SnapshotError::InvalidManifest("truncated integer"))?;
    *offset += 2;
    Ok(u16::from_be_bytes(value.try_into().expect("fixed range")))
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, SnapshotError> {
    let value = bytes
        .get(*offset..*offset + 8)
        .ok_or(SnapshotError::InvalidManifest("truncated integer"))?;
    *offset += 8;
    Ok(u64::from_be_bytes(value.try_into().expect("fixed range")))
}

fn read_digest(bytes: &[u8], offset: &mut usize) -> Result<[u8; 32], SnapshotError> {
    let value = bytes
        .get(*offset..*offset + 32)
        .ok_or(SnapshotError::InvalidManifest("truncated digest"))?;
    *offset += 32;
    Ok(value.try_into().expect("fixed range"))
}

fn validate_inventory<I: Ord>(
    inventory: &[InventoryEntry<I>],
    max_count: usize,
    max_item_bytes: usize,
) -> Result<(), SnapshotError> {
    if inventory.len() > max_count {
        return Err(SnapshotError::LimitExceeded("inventory count"));
    }
    let mut previous = None;
    for item in inventory {
        if item.bytes > max_item_bytes as u64 {
            return Err(SnapshotError::LimitExceeded("item bytes"));
        }
        if previous.is_some_and(|id| id >= &item.id) {
            return Err(SnapshotError::InvalidManifest(
                "inventory is not strictly ordered",
            ));
        }
        previous = Some(&item.id);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotId(pub [u8; 32]);

pub type ObjectBatch = Vec<(ObjectId, Vec<u8>)>;
pub type ContentBatch = Vec<(ContentId, Vec<u8>)>;

/// A completed snapshot source. `read_manifest` is authoritative: implementations must not expose
/// it until every payload batch is durable. Batch reads are stateless, making retry resumable.
#[async_trait]
pub trait SnapshotSource: Send + Sync {
    async fn read_manifest(&self) -> Result<(SnapshotId, Vec<u8>), SnapshotError>;
    async fn read_objects(
        &self,
        inventory: &[InventoryEntry<ObjectId>],
    ) -> Result<ObjectBatch, SnapshotError>;
    async fn read_contents(
        &self,
        inventory: &[InventoryEntry<ContentId>],
    ) -> Result<ContentBatch, SnapshotError>;
}

/// A staging snapshot sink. Payload calls are bounded and retry-idempotent; the manifest is the
/// final commit record and must become visible atomically only after all prior writes are durable.
#[async_trait]
pub trait SnapshotSink: Send + Sync {
    async fn write_objects(&self, objects: &[(ObjectId, Vec<u8>)]) -> Result<(), SnapshotError>;
    async fn write_contents(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), SnapshotError>;
    async fn commit_manifest(
        &self,
        id: SnapshotId,
        canonical_bytes: &[u8],
    ) -> Result<(), SnapshotError>;
}

#[derive(Clone, Debug)]
pub struct NativeSnapshot {
    pub manifest_id: SnapshotId,
    pub manifest: SnapshotManifest,
    pub objects: ObjectBatch,
    pub contents: ContentBatch,
}

#[async_trait]
impl SnapshotSource for NativeSnapshot {
    async fn read_manifest(&self) -> Result<(SnapshotId, Vec<u8>), SnapshotError> {
        Ok((self.manifest_id, self.manifest.canonical_bytes()))
    }

    async fn read_objects(
        &self,
        inventory: &[InventoryEntry<ObjectId>],
    ) -> Result<ObjectBatch, SnapshotError> {
        select_batch(&self.objects, inventory)
    }

    async fn read_contents(
        &self,
        inventory: &[InventoryEntry<ContentId>],
    ) -> Result<ContentBatch, SnapshotError> {
        select_batch(&self.contents, inventory)
    }
}

fn select_batch<I: Copy + Ord>(
    payload: &[(I, Vec<u8>)],
    inventory: &[InventoryEntry<I>],
) -> Result<Vec<(I, Vec<u8>)>, SnapshotError> {
    let Some(first) = inventory.first() else {
        return Ok(Vec::new());
    };
    let start = payload
        .binary_search_by_key(&first.id, |item| item.0)
        .map_err(|_| SnapshotError::InventoryMismatch)?;
    let end = start
        .checked_add(inventory.len())
        .ok_or(SnapshotError::InventoryMismatch)?;
    let selected = payload
        .get(start..end)
        .ok_or(SnapshotError::InventoryMismatch)?;
    Ok(selected.to_vec())
}

#[derive(Debug, Default)]
pub struct MemorySnapshotSink {
    manifest: Mutex<Option<(SnapshotId, Vec<u8>)>>,
    objects: Mutex<ObjectBatch>,
    contents: Mutex<ContentBatch>,
}

#[async_trait]
impl SnapshotSink for MemorySnapshotSink {
    async fn write_objects(&self, batch: &[(ObjectId, Vec<u8>)]) -> Result<(), SnapshotError> {
        merge_memory_batch(
            &mut self.objects.lock().expect("snapshot mutex poisoned"),
            batch,
        )
    }

    async fn write_contents(&self, batch: &[(ContentId, Vec<u8>)]) -> Result<(), SnapshotError> {
        merge_memory_batch(
            &mut self.contents.lock().expect("snapshot mutex poisoned"),
            batch,
        )
    }

    async fn commit_manifest(&self, id: SnapshotId, bytes: &[u8]) -> Result<(), SnapshotError> {
        let mut manifest = self.manifest.lock().expect("snapshot mutex poisoned");
        match manifest.as_ref() {
            Some((existing_id, existing)) if *existing_id != id || existing != bytes => {
                Err(SnapshotError::InventoryMismatch)
            }
            Some(_) => Ok(()),
            None => {
                *manifest = Some((id, bytes.to_vec()));
                Ok(())
            }
        }
    }
}

fn merge_memory_batch<I: Copy + Ord>(
    target: &mut Vec<(I, Vec<u8>)>,
    batch: &[(I, Vec<u8>)],
) -> Result<(), SnapshotError> {
    for (id, bytes) in batch {
        match target.binary_search_by_key(id, |item| item.0) {
            Ok(index) if target[index].1 == *bytes => {}
            Ok(_) => return Err(SnapshotError::InventoryMismatch),
            Err(index) => target.insert(index, (*id, bytes.clone())),
        }
    }
    Ok(())
}

impl MemorySnapshotSink {
    pub fn into_snapshot(self, limits: SnapshotLimits) -> Result<NativeSnapshot, SnapshotError> {
        let (manifest_id, bytes) = self
            .manifest
            .into_inner()
            .expect("snapshot mutex poisoned")
            .ok_or(SnapshotError::Incomplete)?;
        let manifest = SnapshotManifest::decode(&bytes, limits)?;
        if manifest.id() != manifest_id {
            return Err(SnapshotError::InvalidManifest("authentication failed"));
        }
        let snapshot = NativeSnapshot {
            manifest_id,
            manifest,
            objects: self.objects.into_inner().expect("snapshot mutex poisoned"),
            contents: self.contents.into_inner().expect("snapshot mutex poisoned"),
        };
        validate_native_payload(&snapshot, limits)?;
        Ok(snapshot)
    }
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("invalid snapshot manifest: {0}")]
    InvalidManifest(&'static str),
    #[error("snapshot has no committed manifest")]
    Incomplete,
    #[error("snapshot payload is missing or conflicts with its exact inventory")]
    InventoryMismatch,
    #[error("snapshot source or sink violated batch bounds")]
    BatchBounds,
    #[error("destination has conflicting publication authority")]
    ConflictingAuthority,
    #[error("snapshot traversal scratch file failed")]
    ScratchFile(#[source] std::io::Error),
    #[error("snapshot traversal scratch index failed")]
    Scratch(#[from] rusqlite::Error),
    #[error("snapshot closure object does not decode")]
    Decode(#[from] wheat_core::CanonicalObjectError),
    #[error(transparent)]
    Object(#[from] ObjectStoreError),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error(transparent)]
    Publication(#[from] PublicationError),
    #[error(transparent)]
    Repository(#[from] RepositoryPublicationError),
}

/// Convenience adapter which intentionally aggregates payloads in memory. Production callers
/// should use [`export_snapshot_to`].
pub async fn export_snapshot<O: ImmutableObjectStore, P: PublicationStore>(
    objects: &O,
    content: &ContentStore,
    publications: &P,
    limits: SnapshotLimits,
) -> Result<NativeSnapshot, SnapshotError> {
    let sink = MemorySnapshotSink::default();
    export_snapshot_to(objects, content, publications, &sink, limits).await?;
    sink.into_snapshot(limits)
}

/// Streams bounded payload batches and commits the authoritative manifest last.
pub async fn export_snapshot_to<O: ImmutableObjectStore, P: PublicationStore, S: SnapshotSink>(
    objects: &O,
    content: &ContentStore,
    publications: &P,
    sink: &S,
    limits: SnapshotLimits,
) -> Result<SnapshotId, SnapshotError> {
    validate_limits(limits)?;
    let head = publications.read()?;
    let mut walk = ClosureWalk::new(limits)?;
    walk.enqueue_object(head.envelope.into())?;
    let mut total = 0usize;
    let mut object_batch = Vec::with_capacity(limits.batch_items);
    let mut batch_bytes = 0usize;

    while let Some(id) = walk.next_object()? {
        let stored = objects.get(id).await?;
        let len = stored.canonical_bytes.len();
        account(
            &mut total,
            len,
            limits.max_object_bytes,
            limits.max_total_bytes,
        )?;
        walk.record_object(id, len)?;
        walk.follow(&stored)?;
        if should_flush(&object_batch, batch_bytes, len, limits)? {
            sink.write_objects(&object_batch).await?;
            object_batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += len;
        object_batch.push((id, stored.canonical_bytes.to_vec()));
    }
    if !object_batch.is_empty() {
        sink.write_objects(&object_batch).await?;
    }

    let mut content_batch = Vec::with_capacity(limits.batch_items);
    batch_bytes = 0;
    while let Some(id) = walk.next_content()? {
        let bytes = content.get_bounded(id, limits.max_content_bytes.min(limits.batch_bytes))?;
        account(
            &mut total,
            bytes.len(),
            limits.max_content_bytes,
            limits.max_total_bytes,
        )?;
        walk.record_content(id, bytes.len())?;
        if should_flush(&content_batch, batch_bytes, bytes.len(), limits)? {
            sink.write_contents(&content_batch).await?;
            content_batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += bytes.len();
        content_batch.push((id, bytes));
    }
    if !content_batch.is_empty() {
        sink.write_contents(&content_batch).await?;
    }

    // The lazy walk bounds every read before semantic verification is allowed to revisit the graph.
    RepositoryReader::new(objects, content)
        .validate_envelope(head.envelope)
        .await?;
    let manifest = SnapshotManifest {
        head,
        envelope: head.envelope,
        objects: walk.object_inventory()?,
        contents: walk.content_inventory()?,
    };
    manifest.validate(limits)?;
    let bytes = manifest.canonical_bytes();
    let id = SnapshotId(*blake3::hash(&bytes).as_bytes());
    sink.commit_manifest(id, &bytes).await?;
    Ok(id)
}

struct ClosureWalk {
    connection: Connection,
    _file: NamedTempFile,
    limits: SnapshotLimits,
    object_count: usize,
    content_count: usize,
}

impl ClosureWalk {
    fn new(limits: SnapshotLimits) -> Result<Self, SnapshotError> {
        let file = NamedTempFile::new().map_err(SnapshotError::ScratchFile)?;
        let connection = Connection::open(file.path())?;
        connection.execute_batch(
            "PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -2048;
             CREATE TABLE objects (
                 id BLOB PRIMARY KEY,
                 processed INTEGER NOT NULL,
                 bytes INTEGER,
                 radix_path BLOB
             );
             CREATE TABLE contents (id BLOB PRIMARY KEY, processed INTEGER NOT NULL, bytes INTEGER);",
        )?;
        Ok(Self {
            connection,
            _file: file,
            limits,
            object_count: 0,
            content_count: 0,
        })
    }

    fn enqueue_object(&mut self, id: ObjectId) -> Result<(), SnapshotError> {
        self.enqueue_object_at(id, None)
    }

    fn enqueue_radix_root(&mut self, id: ObjectId) -> Result<(), SnapshotError> {
        self.enqueue_object_at(id, Some(&[]))
    }

    fn enqueue_radix_child(&mut self, id: ObjectId, path: &[u8]) -> Result<(), SnapshotError> {
        self.enqueue_object_at(id, Some(path))
    }

    fn enqueue_object_at(
        &mut self,
        id: ObjectId,
        radix_path: Option<&[u8]>,
    ) -> Result<(), SnapshotError> {
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO objects (id, processed, radix_path) VALUES (?1, 0, ?2)",
            params![id.as_bytes().as_slice(), radix_path],
        )?;
        if inserted != 0 {
            self.object_count += 1;
            self.check_inventory_limits()?;
        } else if let Some(path) = radix_path {
            let existing = self.connection.query_row(
                "SELECT radix_path FROM objects WHERE id = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )?;
            match existing {
                None => {
                    self.connection.execute(
                        "UPDATE objects SET radix_path = ?2, processed = 0 WHERE id = ?1",
                        params![id.as_bytes().as_slice(), path],
                    )?;
                }
                Some(existing) if existing == path => {}
                Some(_) => {
                    return Err(SnapshotError::InvalidManifest(
                        "radix node reached at multiple paths",
                    ));
                }
            }
        }
        Ok(())
    }

    fn enqueue_content(&mut self, id: ContentId) -> Result<(), SnapshotError> {
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO contents (id, processed) VALUES (?1, 0)",
            [id.as_bytes().as_slice()],
        )?;
        if inserted != 0 {
            self.content_count += 1;
            self.check_inventory_limits()?;
        }
        Ok(())
    }

    fn check_inventory_limits(&self) -> Result<(), SnapshotError> {
        if self.object_count > self.limits.max_objects
            || self.content_count > self.limits.max_contents
        {
            return Err(SnapshotError::LimitExceeded("inventory count"));
        }
        let encoded = MANIFEST_FIXED_BYTES
            .checked_add(
                self.object_count
                    .checked_add(self.content_count)
                    .and_then(|count| count.checked_mul(INVENTORY_ENTRY_BYTES))
                    .ok_or(SnapshotError::LimitExceeded("manifest bytes"))?,
            )
            .ok_or(SnapshotError::LimitExceeded("manifest bytes"))?;
        if encoded > self.limits.max_manifest_bytes {
            return Err(SnapshotError::LimitExceeded("manifest bytes"));
        }
        Ok(())
    }

    fn next_object(&self) -> Result<Option<ObjectId>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT id FROM objects WHERE processed = 0 LIMIT 1")?;
        let mut rows = statement.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.get(0)?;
        let id = ObjectId::from_digest(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SnapshotError::InventoryMismatch)?,
        );
        drop(rows);
        drop(statement);
        self.connection.execute(
            "UPDATE objects SET processed = 1 WHERE id = ?1",
            [id.as_bytes().as_slice()],
        )?;
        Ok(Some(id))
    }

    fn next_content(&self) -> Result<Option<ContentId>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT id FROM contents WHERE processed = 0 LIMIT 1")?;
        let mut rows = statement.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.get(0)?;
        let id = ContentId::from_digest(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SnapshotError::InventoryMismatch)?,
        );
        drop(rows);
        drop(statement);
        self.connection.execute(
            "UPDATE contents SET processed = 1 WHERE id = ?1",
            [id.as_bytes().as_slice()],
        )?;
        Ok(Some(id))
    }

    fn record_object(&self, id: ObjectId, bytes: usize) -> Result<(), SnapshotError> {
        self.connection.execute(
            "UPDATE objects SET bytes = ?2 WHERE id = ?1",
            params![
                id.as_bytes().as_slice(),
                i64::try_from(bytes).map_err(|_| SnapshotError::LimitExceeded("item bytes"))?
            ],
        )?;
        Ok(())
    }

    fn record_content(&self, id: ContentId, bytes: usize) -> Result<(), SnapshotError> {
        self.connection.execute(
            "UPDATE contents SET bytes = ?2 WHERE id = ?1",
            params![
                id.as_bytes().as_slice(),
                i64::try_from(bytes).map_err(|_| SnapshotError::LimitExceeded("item bytes"))?
            ],
        )?;
        Ok(())
    }

    fn object_inventory(&self) -> Result<Vec<InventoryEntry<ObjectId>>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, bytes FROM objects ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            let id: Vec<u8> = row.get(0)?;
            let bytes: i64 = row.get(1)?;
            Ok((id, bytes))
        })?;
        let mut inventory = Vec::with_capacity(self.object_count);
        for row in rows {
            let (id, bytes) = row?;
            inventory.push(InventoryEntry {
                id: ObjectId::from_digest(
                    id.as_slice()
                        .try_into()
                        .map_err(|_| SnapshotError::InventoryMismatch)?,
                ),
                bytes: u64::try_from(bytes).map_err(|_| SnapshotError::InventoryMismatch)?,
            });
        }
        Ok(inventory)
    }

    fn content_inventory(&self) -> Result<Vec<InventoryEntry<ContentId>>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, bytes FROM contents ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            let id: Vec<u8> = row.get(0)?;
            let bytes: i64 = row.get(1)?;
            Ok((id, bytes))
        })?;
        let mut inventory = Vec::with_capacity(self.content_count);
        for row in rows {
            let (id, bytes) = row?;
            inventory.push(InventoryEntry {
                id: ContentId::from_digest(
                    id.as_slice()
                        .try_into()
                        .map_err(|_| SnapshotError::InventoryMismatch)?,
                ),
                bytes: u64::try_from(bytes).map_err(|_| SnapshotError::InventoryMismatch)?,
            });
        }
        Ok(inventory)
    }

    fn radix_path(&self, id: ObjectId) -> Result<Option<Vec<u8>>, SnapshotError> {
        self.connection
            .query_row(
                "SELECT radix_path FROM objects WHERE id = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn follow(&mut self, stored: &StoredObject) -> Result<(), SnapshotError> {
        let bytes = &stored.canonical_bytes;
        match stored.kind {
            ObjectKind::RepositoryEnvelope => {
                let record = wheat_core::decode_repository_envelope_record(bytes)?;
                self.enqueue_object(record.state.into())?;
                self.enqueue_object(record.labels.into())?;
            }
            ObjectKind::ViewLabelManifest => {
                let record = wheat_core::decode_view_label_manifest_record(bytes)?;
                for label in record.labels.values() {
                    self.enqueue_object(label.view.into())?;
                }
            }
            ObjectKind::RepositoryState => {
                let record = wheat_core::decode_repository_state_record(bytes)?;
                self.enqueue_object(record.base_view.into())?;
                for id in [
                    record.changes.into(),
                    record.view_lineage.into(),
                    record.retained_changes.into(),
                    record.retained_views.into(),
                    record.retained_revisions.into(),
                    record.retained_applications.into(),
                ] {
                    self.enqueue_radix_root(id)?;
                }
                self.enqueue_object(record.required_content.into())?;
            }
            ObjectKind::View => {
                let record = wheat_core::decode_view_record(bytes)?;
                self.enqueue_object(record.tree.into())?;
                for id in [
                    record.revisions.into(),
                    record.applications.into(),
                    record.unresolved_conflicts.into(),
                ] {
                    self.enqueue_radix_root(id)?;
                }
                self.enqueue_object(record.required_content.into())?;
            }
            ObjectKind::ContentManifest => {
                self.enqueue_radix_root(
                    wheat_core::decode_content_manifest_record(bytes)?
                        .content
                        .into(),
                )?;
            }
            ObjectKind::Tree => {
                let node = wheat_core::decode_directory_node(bytes)?;
                for entry in node.entries.values() {
                    match entry {
                        DirectoryEntry::File(value) => self.enqueue_content(value.content)?,
                        DirectoryEntry::Directory(id) => self.enqueue_object((*id).into())?,
                    }
                }
            }
            ObjectKind::Revision => {
                let record = wheat_core::decode_revision_record(bytes)?;
                if let Some(id) = record.predecessor {
                    self.enqueue_object(id.into())?;
                }
                self.enqueue_object(record.authored_against.into())?;
                self.enqueue_object(record.resulting_tree.into())?;
                for id in &record.edits {
                    self.enqueue_object((*id).into())?;
                }
                for resolution in &record.transition.resolutions {
                    self.enqueue_object(resolution.application.into())?;
                    for id in resolution
                        .conflicted_edits
                        .iter()
                        .chain(&resolution.replacement_edits)
                    {
                        self.enqueue_object((*id).into())?;
                    }
                }
                for subsumption in &record.transition.subsumed {
                    self.enqueue_object(subsumption.source.into())?;
                    for id in &subsumption.replacements {
                        self.enqueue_object((*id).into())?;
                    }
                }
            }
            ObjectKind::Application => {
                let record = wheat_core::decode_application_record(bytes)?;
                self.enqueue_object(record.revision_id.into())?;
                self.enqueue_object(record.target_view.into())?;
                match record.result {
                    ApplicationResultRecord::Clean { tree } => self.enqueue_object(tree.into())?,
                    ApplicationResultRecord::Conflicted {
                        partial_tree,
                        conflict,
                    } => {
                        self.enqueue_object(partial_tree.into())?;
                        self.enqueue_object(conflict.into())?;
                    }
                }
                for (edit, evidence) in record.applied {
                    self.enqueue_object(edit.into())?;
                    if let Some(value) = evidence.target {
                        self.enqueue_content(value.content)?;
                    }
                }
            }
            ObjectKind::Conflict => {
                let record = wheat_core::decode_conflict(bytes)?;
                self.enqueue_object(record.base_view.into())?;
                for (edit, evidence) in record.edits {
                    self.enqueue_object(edit.into())?;
                    if let Some(value) = evidence.target {
                        self.enqueue_content(value.content)?;
                    }
                    if let EditFailure::Interference { with } = evidence.reason {
                        for id in with {
                            self.enqueue_object(id.into())?;
                        }
                    }
                }
            }
            ObjectKind::Edit => match wheat_core::decode_edit_record(bytes)?.operation {
                EditOperation::GuardedText {
                    before,
                    expected,
                    replacement,
                    after,
                    ..
                } => {
                    for id in [before, expected, replacement, after] {
                        self.enqueue_content(id)?;
                    }
                }
                EditOperation::ExactFile {
                    expected,
                    replacement,
                } => {
                    for value in expected.into_iter().chain(replacement) {
                        self.enqueue_content(value.content)?;
                    }
                }
            },
            ObjectKind::Change => {}
            ObjectKind::RevisionMapNode => {
                let node = wheat_core::decode_revision_map_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    node.value.map(Into::into),
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    false,
                )?;
            }
            ObjectKind::ApplicationMapNode => {
                let node = wheat_core::decode_application_map_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    node.value.map(Into::into),
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    false,
                )?;
            }
            ObjectKind::ChangeMapNode => {
                let node = wheat_core::decode_change_map_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    node.value.map(Into::into),
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    false,
                )?;
            }
            ObjectKind::ViewLineageMapNode => {
                let node = wheat_core::decode_view_lineage_map_node(bytes)?;
                let child_set = node.value;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    None,
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    false,
                )?;
                if let Some(id) = child_set {
                    self.enqueue_radix_root(id.into())?;
                }
            }
            ObjectKind::ConflictSetNode => {
                let node = wheat_core::decode_conflict_set_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    None,
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    node.present,
                )?;
            }
            ObjectKind::ContentSetNode => {
                let node = wheat_core::decode_content_set_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    None,
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    node.present,
                    false,
                )?;
            }
            ObjectKind::RetainedSetNode => {
                let node = wheat_core::decode_retained_set_node(bytes)?;
                self.follow_radix(
                    stored.id,
                    &node.prefix,
                    None,
                    node.children
                        .into_iter()
                        .map(|child| (child.edge, child.child.into())),
                    false,
                    node.present,
                )?;
            }
        }
        Ok(())
    }

    fn follow_radix(
        &mut self,
        id: ObjectId,
        prefix: &[u8],
        value: Option<ObjectId>,
        children: impl IntoIterator<Item = (u8, ObjectId)>,
        content_terminal: bool,
        object_terminal: bool,
    ) -> Result<(), SnapshotError> {
        let Some(mut path) = self.radix_path(id)? else {
            // The same immutable object can also be a semantic value elsewhere. If it is later
            // reached structurally, enqueue_object_at resets it with the authenticated path.
            return Ok(());
        };
        path.extend_from_slice(prefix);
        if let Some(value) = value {
            self.enqueue_object(value)?;
        }
        if content_terminal || object_terminal {
            let digest: [u8; 32] = path
                .as_slice()
                .try_into()
                .map_err(|_| SnapshotError::InvalidManifest("invalid set terminal"))?;
            if content_terminal {
                self.enqueue_content(ContentId::from_digest(digest))?;
            } else {
                self.enqueue_object(ObjectId::from_digest(digest))?;
            }
        }
        for (edge, child) in children {
            path.push(edge);
            self.enqueue_radix_child(child, &path)?;
            path.pop();
        }
        Ok(())
    }
}

fn should_flush<I>(
    batch: &[(I, Vec<u8>)],
    batch_bytes: usize,
    next_bytes: usize,
    limits: SnapshotLimits,
) -> Result<bool, SnapshotError> {
    if next_bytes > limits.batch_bytes {
        return Err(SnapshotError::LimitExceeded("item exceeds batch bytes"));
    }
    Ok(!batch.is_empty()
        && (batch.len() == limits.batch_items
            || batch_bytes.saturating_add(next_bytes) > limits.batch_bytes))
}

pub async fn restore_snapshot<O: ImmutableObjectStore, P: PublicationStore>(
    snapshot: &NativeSnapshot,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    limits: SnapshotLimits,
) -> Result<RepositoryHead, SnapshotError> {
    restore_snapshot_from(snapshot, objects, content, publications, limits).await
}

/// Restores from a bounded source. The head is published only after every declared payload has
/// been authenticated, durably written, and the resulting repository closure validates.
pub async fn restore_snapshot_from<
    S: SnapshotSource,
    O: ImmutableObjectStore,
    P: PublicationStore,
>(
    source: &S,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    limits: SnapshotLimits,
) -> Result<RepositoryHead, SnapshotError> {
    validate_limits(limits)?;
    let (claimed, manifest_bytes) = source.read_manifest().await?;
    let manifest = SnapshotManifest::decode(&manifest_bytes, limits)?;
    if manifest.id() != claimed {
        return Err(SnapshotError::InvalidManifest("authentication failed"));
    }
    match publications.read() {
        Err(PublicationError::Missing) => {}
        Ok(actual) if actual == manifest.head => {}
        Ok(_) => return Err(SnapshotError::ConflictingAuthority),
        Err(error) => return Err(error.into()),
    }

    let mut start = 0;
    while start < manifest.objects.len() {
        let end = inventory_batch_end(&manifest.objects, start, limits)?;
        let inventory = &manifest.objects[start..end];
        let batch = source.read_objects(inventory).await?;
        validate_object_batch(&batch, inventory, limits)?;
        objects.put_batch(&batch, 16).await?;
        start = end;
    }
    start = 0;
    while start < manifest.contents.len() {
        let end = inventory_batch_end(&manifest.contents, start, limits)?;
        let inventory = &manifest.contents[start..end];
        let batch = source.read_contents(inventory).await?;
        validate_content_batch(&batch, inventory, limits)?;
        content.put_many(batch, 16)?;
        start = end;
    }
    let reader = RepositoryReader::new(objects, content);
    reader.validate_envelope(manifest.envelope).await?;
    let actual = exact_closure_inventory(objects, content, manifest.envelope, limits).await?;
    if actual.objects != manifest.objects || actual.contents != manifest.contents {
        return Err(SnapshotError::InventoryMismatch);
    }
    publications.initialize(manifest.head)?;
    Ok(manifest.head)
}

async fn exact_closure_inventory<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    envelope: RepositoryEnvelopeId,
    limits: SnapshotLimits,
) -> Result<SnapshotManifest, SnapshotError> {
    let mut walk = ClosureWalk::new(limits)?;
    walk.enqueue_object(envelope.into())?;
    let mut total = 0usize;
    while let Some(id) = walk.next_object()? {
        let stored = objects.get(id).await?;
        let len = stored.canonical_bytes.len();
        account(
            &mut total,
            len,
            limits.max_object_bytes,
            limits.max_total_bytes,
        )?;
        walk.record_object(id, len)?;
        walk.follow(&stored)?;
    }
    while let Some(id) = walk.next_content()? {
        let bytes = content.get_bounded(id, limits.max_content_bytes)?;
        account(
            &mut total,
            bytes.len(),
            limits.max_content_bytes,
            limits.max_total_bytes,
        )?;
        walk.record_content(id, bytes.len())?;
    }
    Ok(SnapshotManifest {
        head: RepositoryHead::initial(envelope),
        envelope,
        objects: walk.object_inventory()?,
        contents: walk.content_inventory()?,
    })
}

fn validate_native_payload(
    snapshot: &NativeSnapshot,
    limits: SnapshotLimits,
) -> Result<(), SnapshotError> {
    if snapshot.objects.len() != snapshot.manifest.objects.len()
        || snapshot.contents.len() != snapshot.manifest.contents.len()
    {
        return Err(SnapshotError::InventoryMismatch);
    }
    for ((id, bytes), entry) in snapshot.objects.iter().zip(&snapshot.manifest.objects) {
        if id != &entry.id
            || bytes.len() as u64 != entry.bytes
            || ObjectId::of_canonical_bytes(bytes) != *id
        {
            return Err(SnapshotError::InventoryMismatch);
        }
        wheat_core::validate_canonical_object(bytes).map_err(ObjectStoreError::from)?;
    }
    for ((id, bytes), entry) in snapshot.contents.iter().zip(&snapshot.manifest.contents) {
        if id != &entry.id || bytes.len() as u64 != entry.bytes || ContentId::of(bytes) != *id {
            return Err(SnapshotError::InventoryMismatch);
        }
    }
    snapshot.manifest.validate(limits)
}

fn validate_batch_shape<I: Eq>(
    batch: &[(I, Vec<u8>)],
    inventory: &[InventoryEntry<I>],
    limits: SnapshotLimits,
) -> Result<(), SnapshotError> {
    let bytes = batch
        .iter()
        .try_fold(0usize, |sum, item| sum.checked_add(item.1.len()))
        .ok_or(SnapshotError::BatchBounds)?;
    if batch.len() > limits.batch_items || bytes > limits.batch_bytes {
        return Err(SnapshotError::BatchBounds);
    }
    if batch.len() != inventory.len()
        || batch
            .iter()
            .zip(inventory)
            .any(|((id, bytes), entry)| id != &entry.id || bytes.len() as u64 != entry.bytes)
    {
        return Err(SnapshotError::InventoryMismatch);
    }
    Ok(())
}

fn validate_object_batch(
    batch: &[(ObjectId, Vec<u8>)],
    inventory: &[InventoryEntry<ObjectId>],
    limits: SnapshotLimits,
) -> Result<(), SnapshotError> {
    validate_batch_shape(batch, inventory, limits)?;
    for (id, bytes) in batch {
        if ObjectId::of_canonical_bytes(bytes) != *id {
            return Err(SnapshotError::InventoryMismatch);
        }
        wheat_core::validate_canonical_object(bytes).map_err(ObjectStoreError::from)?;
    }
    Ok(())
}

fn validate_content_batch(
    batch: &[(ContentId, Vec<u8>)],
    inventory: &[InventoryEntry<ContentId>],
    limits: SnapshotLimits,
) -> Result<(), SnapshotError> {
    validate_batch_shape(batch, inventory, limits)?;
    if batch.iter().any(|(id, bytes)| ContentId::of(bytes) != *id) {
        return Err(SnapshotError::InventoryMismatch);
    }
    Ok(())
}

fn inventory_batch_end<I>(
    items: &[InventoryEntry<I>],
    start: usize,
    limits: SnapshotLimits,
) -> Result<usize, SnapshotError> {
    let mut end = start;
    let mut bytes = 0usize;
    while end < items.len() && end - start < limits.batch_items {
        let next = usize::try_from(items[end].bytes)
            .map_err(|_| SnapshotError::LimitExceeded("item bytes"))?;
        if next > limits.batch_bytes {
            return Err(SnapshotError::LimitExceeded("item exceeds batch bytes"));
        }
        if end > start && bytes.saturating_add(next) > limits.batch_bytes {
            break;
        }
        bytes += next;
        end += 1;
    }
    Ok(end)
}

fn validate_limits(limits: SnapshotLimits) -> Result<(), SnapshotError> {
    if limits.batch_items == 0
        || limits.batch_bytes == 0
        || limits.max_manifest_bytes < MANIFEST_FIXED_BYTES
    {
        return Err(SnapshotError::LimitExceeded("batch or manifest bound"));
    }
    Ok(())
}

fn account(
    total: &mut usize,
    bytes: usize,
    item_max: usize,
    total_max: usize,
) -> Result<(), SnapshotError> {
    if bytes > item_max {
        return Err(SnapshotError::LimitExceeded("item bytes"));
    }
    *total = total
        .checked_add(bytes)
        .ok_or(SnapshotError::LimitExceeded("total bytes"))?;
    if *total > total_max {
        return Err(SnapshotError::LimitExceeded("total bytes"));
    }
    Ok(())
}
