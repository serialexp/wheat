//! Bounded, authenticated native repository snapshots.
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{Connection, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};

use crate::{
    ContentStore, ContentStoreError, ImmutableObjectStore, ObjectStoreError, PublicationError,
    PublicationStore, RepositoryHead, RepositoryPublicationError, RepositoryReader, StoredObject,
    object_graph::{self, MalformedRadix, Reference},
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

impl From<MalformedRadix> for SnapshotError {
    fn from(_: MalformedRadix) -> Self {
        Self::InvalidManifest("radix node does not fit where it is reached")
    }
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

    while let Some(visit) = walk.next_object()? {
        let stored = objects.get(visit.id).await?;
        let len = stored.canonical_bytes.len();
        if visit.first {
            account(
                &mut total,
                len,
                limits.max_object_bytes,
                limits.max_total_bytes,
            )?;
            walk.record_object(visit.id, len)?;
        }
        walk.follow(&stored, &visit)?;
        if !visit.first {
            continue;
        }
        if should_flush(&object_batch, batch_bytes, len, limits)? {
            sink.write_objects(&object_batch).await?;
            object_batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += len;
        object_batch.push((visit.id, stored.canonical_bytes.to_vec()));
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

/// How many visits the walk allows per object in the inventory limit. A radix node's
/// ID does not include where it sits, so one node can sit under several edges and is
/// followed once for each; a crafted set could repeat that at every level.
const VISITS_PER_OBJECT: usize = 2;

/// The closure of an envelope, walked with its bookkeeping in a scratch SQLite file so
/// memory stays flat however big the repository is.
///
/// An object is visited once for each place it is reached: once as a plain reference,
/// and once for each key path it sits under as a radix node (a node lists its keys
/// relative to that path). The first visit records it in the inventory.
struct ClosureWalk {
    connection: Connection,
    _file: NamedTempFile,
    limits: SnapshotLimits,
    object_count: usize,
    content_count: usize,
    visit_count: usize,
    /// Scratch for the place being enqueued, encoded as [`encode_place`] does.
    place: Vec<u8>,
}

/// One visit of an object: read it, record it if it is the first, and follow it.
struct Visit {
    id: ObjectId,
    /// Whether this is the object's first visit, the one that records it.
    first: bool,
    /// The key bytes above it, when it was reached as a radix node.
    radix_path: Option<RadixPath>,
}

/// Key bytes above a radix node. Full keys are 32 bytes, so no path is longer.
struct RadixPath {
    bytes: [u8; 32],
    len: usize,
}

impl Visit {
    fn radix_path(&self) -> Option<&[u8]> {
        self.radix_path.as_ref().map(|path| &path.bytes[..path.len])
    }
}

/// A place as stored in the scratch file: empty for a plain reference, `1` and the
/// key path for a radix node.
fn encode_place(place: &mut Vec<u8>, radix_path: Option<&[u8]>) {
    place.clear();
    if let Some(path) = radix_path {
        place.push(1);
        place.extend_from_slice(path);
    }
}

fn decode_place(place: &[u8]) -> Result<Option<RadixPath>, SnapshotError> {
    match place {
        [] => Ok(None),
        [1, path @ ..] if path.len() <= 32 => {
            let mut bytes = [0; 32];
            bytes[..path.len()].copy_from_slice(path);
            Ok(Some(RadixPath {
                bytes,
                len: path.len(),
            }))
        }
        _ => Err(SnapshotError::InventoryMismatch),
    }
}

/// Column `index` of a scratch row, which the walk only ever fills with a blob.
fn blob<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> Result<&'a [u8], SnapshotError> {
    row.get_ref(index)?
        .as_blob()
        .map_err(|_| SnapshotError::InventoryMismatch)
}

fn digest(bytes: &[u8]) -> Result<[u8; 32], SnapshotError> {
    bytes
        .try_into()
        .map_err(|_| SnapshotError::InventoryMismatch)
}

impl ClosureWalk {
    fn new(limits: SnapshotLimits) -> Result<Self, SnapshotError> {
        let file = NamedTempFile::new().map_err(SnapshotError::ScratchFile)?;
        let connection = Connection::open(file.path())?;
        // `pending` and `pending_contents` are stacks: the walk pops the highest
        // `seq`, which the rowid index finds without a scan.
        connection.execute_batch(
            "PRAGMA temp_store = MEMORY;
             PRAGMA journal_mode = OFF;
             PRAGMA synchronous = OFF;
             PRAGMA cache_size = -2048;
             CREATE TABLE objects (id BLOB PRIMARY KEY, bytes INTEGER) WITHOUT ROWID;
             CREATE TABLE visits (
                 id BLOB NOT NULL,
                 place BLOB NOT NULL,
                 PRIMARY KEY (id, place)
             ) WITHOUT ROWID;
             CREATE TABLE pending (
                 seq INTEGER PRIMARY KEY,
                 id BLOB NOT NULL,
                 place BLOB NOT NULL,
                 first INTEGER NOT NULL
             );
             CREATE TABLE contents (id BLOB PRIMARY KEY, bytes INTEGER) WITHOUT ROWID;
             CREATE TABLE pending_contents (seq INTEGER PRIMARY KEY, id BLOB NOT NULL);",
        )?;
        Ok(Self {
            connection,
            _file: file,
            limits,
            object_count: 0,
            content_count: 0,
            visit_count: 0,
            place: Vec::with_capacity(33),
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
        let id = id.as_bytes().as_slice();
        encode_place(&mut self.place, radix_path);
        let new_visit = self
            .connection
            .prepare_cached("INSERT OR IGNORE INTO visits (id, place) VALUES (?1, ?2)")?
            .execute(params![id, self.place])?;
        if new_visit == 0 {
            return Ok(());
        }
        self.visit_count += 1;
        if self.visit_count > self.limits.max_objects.saturating_mul(VISITS_PER_OBJECT) {
            return Err(SnapshotError::LimitExceeded("closure visits"));
        }
        let first = self
            .connection
            .prepare_cached("INSERT OR IGNORE INTO objects (id) VALUES (?1)")?
            .execute([id])?
            != 0;
        if first {
            self.object_count += 1;
            self.check_inventory_limits()?;
        }
        self.connection
            .prepare_cached("INSERT INTO pending (id, place, first) VALUES (?1, ?2, ?3)")?
            .execute(params![id, self.place, first])?;
        Ok(())
    }

    fn enqueue_content(&mut self, id: ContentId) -> Result<(), SnapshotError> {
        let id = id.as_bytes().as_slice();
        let inserted = self
            .connection
            .prepare_cached("INSERT OR IGNORE INTO contents (id) VALUES (?1)")?
            .execute([id])?;
        if inserted != 0 {
            self.content_count += 1;
            self.check_inventory_limits()?;
            self.connection
                .prepare_cached("INSERT INTO pending_contents (id) VALUES (?1)")?
                .execute([id])?;
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

    fn next_object(&self) -> Result<Option<Visit>, SnapshotError> {
        // Every change a RETURNING statement makes happens on its first step, so
        // stopping after the one row it returns is fine.
        let mut statement = self.connection.prepare_cached(
            "DELETE FROM pending WHERE seq = (SELECT max(seq) FROM pending)
             RETURNING id, place, first",
        )?;
        let mut rows = statement.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(Visit {
            id: ObjectId::from_digest(digest(blob(row, 0)?)?),
            radix_path: decode_place(blob(row, 1)?)?,
            first: row.get(2)?,
        }))
    }

    fn next_content(&self) -> Result<Option<ContentId>, SnapshotError> {
        let mut statement = self.connection.prepare_cached(
            "DELETE FROM pending_contents WHERE seq = (SELECT max(seq) FROM pending_contents)
             RETURNING id",
        )?;
        let mut rows = statement.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(ContentId::from_digest(digest(blob(row, 0)?)?)))
    }

    fn record_object(&self, id: ObjectId, bytes: usize) -> Result<(), SnapshotError> {
        self.connection
            .prepare_cached("UPDATE objects SET bytes = ?2 WHERE id = ?1")?
            .execute(params![
                id.as_bytes().as_slice(),
                i64::try_from(bytes).map_err(|_| SnapshotError::LimitExceeded("item bytes"))?
            ])?;
        Ok(())
    }

    fn record_content(&self, id: ContentId, bytes: usize) -> Result<(), SnapshotError> {
        self.connection
            .prepare_cached("UPDATE contents SET bytes = ?2 WHERE id = ?1")?
            .execute(params![
                id.as_bytes().as_slice(),
                i64::try_from(bytes).map_err(|_| SnapshotError::LimitExceeded("item bytes"))?
            ])?;
        Ok(())
    }

    fn object_inventory(&self) -> Result<Vec<InventoryEntry<ObjectId>>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, bytes FROM objects ORDER BY id")?;
        let mut rows = statement.query([])?;
        let mut inventory = Vec::with_capacity(self.object_count);
        while let Some(row) = rows.next()? {
            inventory.push(InventoryEntry {
                id: ObjectId::from_digest(digest(blob(row, 0)?)?),
                bytes: u64::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| SnapshotError::InventoryMismatch)?,
            });
        }
        Ok(inventory)
    }

    fn content_inventory(&self) -> Result<Vec<InventoryEntry<ContentId>>, SnapshotError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, bytes FROM contents ORDER BY id")?;
        let mut rows = statement.query([])?;
        let mut inventory = Vec::with_capacity(self.content_count);
        while let Some(row) = rows.next()? {
            inventory.push(InventoryEntry {
                id: ContentId::from_digest(digest(blob(row, 0)?)?),
                bytes: u64::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| SnapshotError::InventoryMismatch)?,
            });
        }
        Ok(inventory)
    }

    /// Enqueues everything `stored` refers to from where `visit` reached it. A radix
    /// node reached only as a plain reference (a value elsewhere) lists nothing.
    fn follow(&mut self, stored: &StoredObject, visit: &Visit) -> Result<(), SnapshotError> {
        object_graph::for_each_reference(stored, visit.radix_path(), |reference| match reference {
            Reference::Object(id) => self.enqueue_object(id),
            Reference::RadixRoot(id) => self.enqueue_radix_root(id),
            Reference::RadixChild { id, above } => self.enqueue_radix_child(id, above),
            Reference::Content(id) => self.enqueue_content(id),
        })
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
    while let Some(visit) = walk.next_object()? {
        let stored = objects.get(visit.id).await?;
        if visit.first {
            let len = stored.canonical_bytes.len();
            account(
                &mut total,
                len,
                limits.max_object_bytes,
                limits.max_total_bytes,
            )?;
            walk.record_object(visit.id, len)?;
        }
        walk.follow(&stored, &visit)?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use wheat_core::{ChangeId, RevisionId};

    use super::*;
    use crate::{Durability, LooseObjectStore, RetainedSetStore, RevisionMapStore};

    fn store() -> (tempfile::TempDir, LooseObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::with_durability(dir.path(), Durability::Deferred);
        (dir, objects)
    }

    /// What a walk from one radix root found.
    struct Walked {
        /// Every object in the inventory: the nodes and what their keys point at.
        inventory: HashSet<ObjectId>,
        /// The ones not in the store: a set's members or a map's values.
        absent: HashSet<ObjectId>,
        visits: usize,
    }

    /// Walks from `root` the way export does, except that objects missing from the
    /// store are noted instead of read.
    async fn walk_from(
        objects: &LooseObjectStore,
        root: ObjectId,
        limits: SnapshotLimits,
    ) -> Result<Walked, SnapshotError> {
        let mut walk = ClosureWalk::new(limits)?;
        walk.enqueue_radix_root(root)?;
        let mut absent = HashSet::new();
        while let Some(visit) = walk.next_object()? {
            let Ok(stored) = objects.get(visit.id).await else {
                absent.insert(visit.id);
                walk.record_object(visit.id, 0)?;
                continue;
            };
            if visit.first {
                walk.record_object(visit.id, stored.canonical_bytes.len())?;
            }
            walk.follow(&stored, &visit)?;
        }
        Ok(Walked {
            inventory: walk
                .object_inventory()?
                .into_iter()
                .map(|entry| entry.id)
                .collect(),
            absent,
            visits: walk.visit_count,
        })
    }

    fn key(bytes: &[u8]) -> ObjectId {
        let mut digest = [0; 32];
        digest[..bytes.len()].copy_from_slice(bytes);
        ObjectId::from_digest(digest)
    }

    #[tokio::test]
    async fn one_subtree_under_two_edges_is_followed_under_both() {
        let (_dir, objects) = store();
        let keys = [
            key(&[1, 5, 5, 5, 1]),
            key(&[1, 5, 5, 5, 2]),
            key(&[2, 5, 5, 5, 1]),
            key(&[2, 5, 5, 5, 2]),
        ];
        let sets = RetainedSetStore::new(&objects);
        let empty = sets.empty_root().await.unwrap();
        let root = sets
            .update_batch(empty, keys.iter().map(|key| (*key, true)))
            .await
            .unwrap();

        let walked = walk_from(&objects, root.into(), SnapshotLimits::default())
            .await
            .unwrap();
        assert_eq!(walked.absent, keys.into(), "every member is found");
        // The root and one node shared by both edges, then the four members.
        assert_eq!(walked.inventory.len(), 6);
        assert_eq!(walked.visits, 7, "the shared node is followed twice");
    }

    /// A map whose keys branch the same way at three levels, all with one value, is
    /// four nodes and one value, but a walk follows the bottom node eight times.
    #[tokio::test]
    async fn repeated_subtrees_are_capped_by_the_visit_limit() {
        let (_dir, objects) = store();
        let value = RevisionId::from_digest([9; 32]);
        let mut entries = Vec::new();
        for a in 1..=2 {
            for b in 1..=2 {
                for c in 1..=2 {
                    for d in 1..=2 {
                        let mut key = [5; 16];
                        key[..3].copy_from_slice(&[a, b, c]);
                        key[15] = d;
                        entries.push((ChangeId::from_bytes(key), Some(value)));
                    }
                }
            }
        }
        let maps = RevisionMapStore::new(&objects);
        let empty = maps.empty_root().await.unwrap();
        let root = maps.update_batch(empty, entries).await.unwrap();

        let walked = walk_from(&objects, root.into(), SnapshotLimits::default())
            .await
            .unwrap();
        assert_eq!(walked.absent, [value.into()].into());
        assert_eq!(walked.inventory.len(), 5);
        assert_eq!(walked.visits, 1 + 2 + 4 + 8 + 1);

        // Five objects allow ten visits, fewer than the walk needs.
        let limits = SnapshotLimits {
            max_objects: 5,
            ..SnapshotLimits::default()
        };
        let error = walk_from(&objects, root.into(), limits)
            .await
            .err()
            .expect("the walk is refused");
        assert!(
            matches!(error, SnapshotError::LimitExceeded("closure visits")),
            "{error:?}"
        );
    }
}
