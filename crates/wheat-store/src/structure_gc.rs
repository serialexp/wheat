//! What a pool holds that no head reaches: structural garbage collection
//! (`docs/design/wheat-network-maintenance.md`, "Pool GC").
//!
//! Every publish leaves the previous envelope, its label manifest and the old nodes of
//! each set the new state rewrote unreachable, and nothing removes them.
//!
//! - [`analyze_structure`], the dry run, walks the closure of every head of a network,
//!   lists what the store holds, and counts what no head reaches, by kind. It deletes
//!   nothing, and so needs no hold on the network: a publish while it runs can leave
//!   its new objects counted as unreachable, since the walk read the heads before it.
//! - [`collect_structure`] validates each head completely, marks what they reach and
//!   removes the rest, with the network's writers held (Bart, 2026-11-12). Readers
//!   that began before it go on reading what it removed through a [`StorePin`].

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use rusqlite::params;
use thiserror::Error;
use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};

use crate::{
    ImmutableObjectStore, ObjectKind, ObjectStoreError, RepositoryPublicationError,
    RepositoryReader, RepositoryStore, SnapshotError, SnapshotLimits, StoredObject,
    snapshot::ClosureWalk,
};

/// A record a store holds, as its inventory lists it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoredRecord {
    /// A structural object, with its canonical length.
    Object {
        id: ObjectId,
        kind: ObjectKind,
        bytes: u64,
    },
    /// Content a store that keeps it with its objects holds, with its logical length.
    Content { id: ContentId, bytes: u64 },
}

/// A store that can list what it holds.
pub trait StoredInventory: Send + Sync {
    /// Calls `visit` for each record the store holds, in no order. A record kept in
    /// two places may be listed twice. Fails if the store changed in a way that makes
    /// the listing incomplete, so that it can be run again.
    fn for_each_record(&self, visit: &mut dyn FnMut(StoredRecord)) -> Result<(), ObjectStoreError>;
}

/// How many records, and the bytes they take.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub records: u64,
    pub bytes: u64,
}

impl Counts {
    fn add(&mut self, records: u64, bytes: u64) {
        self.records += records;
        self.bytes += bytes;
    }
}

/// What the heads reach, and what they do not, of a pool.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StructureReport {
    /// The heads walked.
    pub roots: usize,
    /// Objects by kind: reached by some head, and by none.
    pub reachable: BTreeMap<u16, Counts>,
    pub unreachable: BTreeMap<u16, Counts>,
    /// Content records the pool keeps beside its objects. Content is collected apart
    /// from the pool (Gothab keeps it elsewhere), so these are only counted.
    pub content: Counts,
    /// Objects a head reaches that the inventory does not list. None in a store that
    /// holds its own objects; some means the walk read through another store.
    pub unlisted: u64,
}

impl StructureReport {
    /// Everything reached.
    #[must_use]
    pub fn total_reachable(&self) -> Counts {
        sum(&self.reachable)
    }

    /// Everything no head reaches.
    #[must_use]
    pub fn total_unreachable(&self) -> Counts {
        sum(&self.unreachable)
    }
}

/// A line per kind of object, then the totals, so an operator sees where the garbage is.
impl std::fmt::Display for StructureReport {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reachable = self.total_reachable();
        let unreachable = self.total_unreachable();
        writeln!(
            out,
            "{} heads reach {} objects ({} bytes); {} objects ({} bytes) are reached by none",
            self.roots, reachable.records, reachable.bytes, unreachable.records, unreachable.bytes
        )?;
        let kinds: std::collections::BTreeSet<u16> = self
            .reachable
            .keys()
            .chain(self.unreachable.keys())
            .copied()
            .collect();
        for kind in kinds {
            let name = ObjectKind::from_code(kind)
                .map_or_else(|| format!("kind {kind}"), |kind| format!("{kind:?}"));
            let reached = self.reachable.get(&kind).copied().unwrap_or_default();
            let unreached = self.unreachable.get(&kind).copied().unwrap_or_default();
            writeln!(
                out,
                "  {name}: {} reached ({} bytes), {} unreachable ({} bytes)",
                reached.records, reached.bytes, unreached.records, unreached.bytes
            )?;
        }
        writeln!(
            out,
            "content kept in the pool: {} records ({} bytes), not counted above",
            self.content.records, self.content.bytes
        )?;
        if self.unlisted > 0 {
            writeln!(
                out,
                "{} reached objects are not in the store's listing",
                self.unlisted
            )?;
        }
        Ok(())
    }
}

fn sum(counts: &BTreeMap<u16, Counts>) -> Counts {
    counts.values().fold(Counts::default(), |mut total, each| {
        total.add(each.records, each.bytes);
        total
    })
}

/// Walks the closure of each of `roots`, the heads of every repository sharing the
/// pool, reading through `objects`, and compares it with what `inventory` lists.
///
/// Memory stays flat however large the pool is: the marks and the listing go to a
/// scratch SQLite file. Fails, with nothing counted, on a head that cannot be read
/// to the end. A walk reads each reachable object once; the listing reads none.
pub async fn analyze_structure<O: ImmutableObjectStore + ?Sized>(
    objects: &O,
    inventory: &dyn StoredInventory,
    roots: &[RepositoryEnvelopeId],
    limits: SnapshotLimits,
) -> Result<StructureReport, SnapshotError> {
    let walk = walk_heads(objects, roots, limits).await?;
    let connection = walk.connection();
    load_inventory(connection, inventory)?;
    summarize(connection, roots.len())
}

/// A pool that can remove what no head reaches: [`collect_structure`].
pub trait CollectableStructure: StoredInventory {
    /// A view of the store as it is now, for a reader that must go on reading what
    /// it began with while a sweep runs (a fetch). What a later sweep removes stays
    /// readable through the pin, and on disk, until the last pin taken before that
    /// sweep is dropped.
    fn pin(&self) -> StorePin;

    /// Removes every object `marks` does not hold. The caller holds every writer
    /// of the store from before the marks were taken until this returns: an object
    /// written meanwhile is not marked, and so is removed.
    fn sweep(&self, marks: &ObjectMarks) -> Result<SweepReport, ObjectStoreError>;
}

/// What a sweep kept and removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
    /// Objects in segments: copied into the new segments, and left out of them.
    pub objects_kept: u64,
    pub object_bytes_kept: u64,
    pub objects_dropped: u64,
    pub object_bytes_dropped: u64,
    /// Objects too large for a segment, kept as files of their own, that were removed.
    pub loose_objects_retired: u64,
    pub loose_object_bytes_retired: u64,
    /// Content records the pool keeps beside its objects, all kept.
    pub content_records_kept: u64,
    /// The segments, and the bytes they take, before and after.
    pub segments_before: u64,
    pub segment_bytes_before: u64,
    pub segments_after: u64,
    pub segment_bytes_after: u64,
}

/// A reader's view of a store from before a sweep: see [`CollectableStructure::pin`].
#[derive(Clone)]
pub struct StorePin(Arc<dyn RemovedObjects>);

impl std::fmt::Debug for StorePin {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("StorePin").finish_non_exhaustive()
    }
}

/// What a sweep removed after a pin was taken, readable through it.
pub(crate) trait RemovedObjects: Send + Sync {
    /// The object `id`, if a sweep after the pin was taken removed it.
    fn removed_object(&self, id: ObjectId) -> Result<Option<StoredObject>, ObjectStoreError>;
}

impl StorePin {
    pub(crate) fn new(removed: Arc<dyn RemovedObjects>) -> Self {
        Self(removed)
    }

    /// The object `id`, if a sweep removed it after this pin was taken.
    pub fn removed_object(&self, id: ObjectId) -> Result<Option<StoredObject>, ObjectStoreError> {
        self.0.removed_object(id)
    }
}

/// An object store read through a [`StorePin`]: an object a sweep removed after the
/// pin was taken is still found. Writes go to the store as they are.
#[derive(Clone, Debug)]
pub struct PinnedObjects<O> {
    objects: O,
    pin: Option<StorePin>,
}

impl<O> PinnedObjects<O> {
    /// `objects`, read through `pin`; as `objects` alone when `pin` is `None`, for a
    /// store no sweep runs on.
    pub fn new(objects: O, pin: Option<StorePin>) -> Self {
        Self { objects, pin }
    }

    pub fn inner(&self) -> &O {
        &self.objects
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> ImmutableObjectStore for PinnedObjects<O> {
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
        match self.objects.get(id).await {
            Err(ObjectStoreError::Missing(missing)) => match &self.pin {
                Some(pin) => pin
                    .removed_object(id)?
                    .ok_or(ObjectStoreError::Missing(missing)),
                None => Err(ObjectStoreError::Missing(missing)),
            },
            result => result,
        }
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut found = self.objects.exists_many(ids).await?;
        if let Some(pin) = &self.pin {
            for (id, found) in ids.iter().zip(&mut found) {
                if !*found {
                    *found = pin.removed_object(*id)?.is_some();
                }
            }
        }
        Ok(found)
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.objects.seal().await
    }
}

/// The objects every head reaches, as the walk marked them on disk: what a sweep
/// keeps. Also records what a sweep has copied, so a record two segments hold is
/// copied once.
pub struct ObjectMarks {
    walk: ClosureWalk,
    marked: u64,
}

impl ObjectMarks {
    fn new(walk: ClosureWalk) -> Result<Self, SnapshotError> {
        let connection = walk.connection();
        connection.execute_batch(
            "CREATE TABLE copied (
                 namespace INTEGER NOT NULL,
                 id BLOB NOT NULL,
                 PRIMARY KEY (namespace, id)
             ) WITHOUT ROWID;",
        )?;
        let marked: i64 =
            connection.query_row("SELECT count(*) FROM objects", [], |row| row.get(0))?;
        Ok(Self {
            walk,
            marked: u64::try_from(marked).unwrap_or(0),
        })
    }

    /// How many objects are marked.
    #[must_use]
    pub fn marked(&self) -> u64 {
        self.marked
    }

    /// Whether a head reaches the object `digest`.
    pub(crate) fn contains(&self, digest: &[u8; 32]) -> rusqlite::Result<bool> {
        self.walk
            .connection()
            .prepare_cached("SELECT 1 FROM objects WHERE id = ?1")?
            .exists([digest.as_slice()])
    }

    /// Records that a sweep copied the record `namespace`/`digest`. False when it
    /// already had.
    pub(crate) fn first_copy(&self, namespace: u8, digest: &[u8; 32]) -> rusqlite::Result<bool> {
        Ok(self
            .walk
            .connection()
            .prepare_cached("INSERT OR IGNORE INTO copied (namespace, id) VALUES (?1, ?2)")?
            .execute(params![namespace, digest.as_slice()])?
            == 1)
    }

    /// Groups what follows into one transaction, until [`Self::commit`]: one per
    /// record would cost most of what the sweep's bookkeeping does.
    pub(crate) fn begin(&self) -> rusqlite::Result<()> {
        self.walk.connection().execute_batch("BEGIN")
    }

    pub(crate) fn commit(&self) -> rusqlite::Result<()> {
        self.walk.connection().execute_batch("COMMIT")
    }
}

/// Why [`collect_structure`] removed nothing.
#[derive(Debug, Error)]
pub enum StructureCollectionError {
    #[error("this store cannot collect what no head reaches")]
    Unsupported,
    #[error("a collection needs at least one head; with none, it would remove everything")]
    NoHeads,
    #[error("head {head} does not validate: {source}")]
    Invalid {
        head: RepositoryEnvelopeId,
        source: RepositoryPublicationError,
    },
    #[error("marking what the heads reach failed: {0}")]
    Mark(#[from] SnapshotError),
    #[error("sweeping failed: {0}")]
    Sweep(#[from] ObjectStoreError),
}

/// What [`collect_structure`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StructureCollection {
    /// The heads validated and walked.
    pub roots: usize,
    /// The objects they reach.
    pub marked: u64,
    pub sweep: SweepReport,
}

impl std::fmt::Display for StructureCollection {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sweep = &self.sweep;
        writeln!(
            out,
            "{} heads validated, reaching {} objects",
            self.roots, self.marked
        )?;
        writeln!(
            out,
            "segments: {} objects ({} bytes) kept, {} objects ({} bytes) removed",
            sweep.objects_kept,
            sweep.object_bytes_kept,
            sweep.objects_dropped,
            sweep.object_bytes_dropped
        )?;
        writeln!(
            out,
            "large objects: {} ({} bytes) removed",
            sweep.loose_objects_retired, sweep.loose_object_bytes_retired
        )?;
        writeln!(
            out,
            "{} segments ({} bytes) replaced by {} ({} bytes); {} content records kept",
            sweep.segments_before,
            sweep.segment_bytes_before,
            sweep.segments_after,
            sweep.segment_bytes_after,
            sweep.content_records_kept
        )
    }
}

/// Removes from `store` every object none of `roots`, the heads of every
/// repository sharing the store, reaches.
///
/// Fails closed, removing nothing, when a head does not validate completely
/// ([`RepositoryReader::validate_envelope`]) or cannot be walked to the end. The
/// caller holds every writer of the store from before this is called until it
/// returns (in a server, wheat-server's `Registry::quiesce`); fetches may go on, reading through
/// a [`CollectableStructure::pin`] taken when they began.
///
/// Reads each head's closure to validate it and once more to mark it, with the
/// marks on disk; writes the kept objects once.
#[tracing::instrument(level = "debug", skip_all, fields(roots = roots.len()))]
pub async fn collect_structure<S: RepositoryStore>(
    store: &S,
    roots: &[RepositoryEnvelopeId],
    limits: SnapshotLimits,
) -> Result<StructureCollection, StructureCollectionError> {
    let collector = store
        .collectable_structure()
        .ok_or(StructureCollectionError::Unsupported)?;
    if roots.is_empty() {
        return Err(StructureCollectionError::NoHeads);
    }
    let content = store.content_store();
    let reader = RepositoryReader::new(store, &content);
    for head in roots {
        reader.validate_envelope(*head).await.map_err(|source| {
            StructureCollectionError::Invalid {
                head: *head,
                source,
            }
        })?;
    }
    let marks = ObjectMarks::new(walk_heads(store, roots, limits).await?)?;
    let sweep = collector.sweep(&marks)?;
    Ok(StructureCollection {
        roots: roots.len(),
        marked: marks.marked(),
        sweep,
    })
}

/// Objects the walk handles per SQLite transaction.
const WALK_TRANSACTION_OBJECTS: usize = 4096;

/// Marks every object the closure of each of `roots` reaches.
#[tracing::instrument(level = "debug", skip_all)]
async fn walk_heads<O: ImmutableObjectStore + ?Sized>(
    objects: &O,
    roots: &[RepositoryEnvelopeId],
    limits: SnapshotLimits,
) -> Result<ClosureWalk, SnapshotError> {
    let mut walk = ClosureWalk::new(limits)?;
    for root in roots {
        walk.enqueue_object((*root).into())?;
    }
    // The walk is a statement or several per object, each its own transaction
    // unless grouped, and a transaction's commit is most of what a statement costs.
    walk.connection().execute_batch("BEGIN")?;
    let mut since_commit = 0_usize;
    while let Some(visit) = walk.next_object()? {
        let stored = objects.get(visit.id).await?;
        walk.follow(&stored, &visit)?;
        since_commit += 1;
        if since_commit == WALK_TRANSACTION_OBJECTS {
            walk.connection().execute_batch("COMMIT; BEGIN")?;
            since_commit = 0;
        }
    }
    walk.connection().execute_batch("COMMIT")?;
    Ok(walk)
}

/// Puts what `inventory` lists into the scratch `stored` table.
#[tracing::instrument(level = "debug", skip_all)]
fn load_inventory(
    connection: &rusqlite::Connection,
    inventory: &dyn StoredInventory,
) -> Result<(), SnapshotError> {
    connection.execute_batch(
        "CREATE TABLE stored (
             content INTEGER NOT NULL,
             id BLOB NOT NULL,
             kind INTEGER NOT NULL,
             bytes INTEGER NOT NULL,
             PRIMARY KEY (content, id)
         ) WITHOUT ROWID;
         BEGIN;",
    )?;
    let mut failure = None;
    {
        let mut insert = connection.prepare(
            "INSERT OR IGNORE INTO stored (content, id, kind, bytes) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut record = |record: StoredRecord| {
            let (content, id, kind, bytes) = match record {
                StoredRecord::Object { id, kind, bytes } => {
                    (0_i64, id.as_bytes().to_vec(), i64::from(kind as u16), bytes)
                }
                StoredRecord::Content { id, bytes } => (1, id.as_bytes().to_vec(), 0, bytes),
            };
            if failure.is_none() {
                let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
                if let Err(error) = insert.execute(params![content, id, kind, bytes]) {
                    failure = Some(error);
                }
            }
        };
        inventory.for_each_record(&mut record)?;
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    connection.execute_batch("COMMIT")?;
    Ok(())
}

/// Groups the `stored` table by kind and by whether the walk reached it.
#[tracing::instrument(level = "debug", skip_all)]
fn summarize(
    connection: &rusqlite::Connection,
    roots: usize,
) -> Result<StructureReport, SnapshotError> {
    let mut report = StructureReport {
        roots,
        ..StructureReport::default()
    };
    let mut statement = connection.prepare(
        "SELECT stored.kind, objects.id IS NOT NULL, count(*), sum(stored.bytes)
         FROM stored LEFT JOIN objects ON objects.id = stored.id
         WHERE stored.content = 0 GROUP BY 1, 2",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let kind =
            u16::try_from(row.get::<_, i64>(0)?).map_err(|_| SnapshotError::InventoryMismatch)?;
        let reached: bool = row.get(1)?;
        let records =
            u64::try_from(row.get::<_, i64>(2)?).map_err(|_| SnapshotError::InventoryMismatch)?;
        let bytes =
            u64::try_from(row.get::<_, i64>(3)?).map_err(|_| SnapshotError::InventoryMismatch)?;
        let side = if reached {
            &mut report.reachable
        } else {
            &mut report.unreachable
        };
        side.entry(kind).or_default().add(records, bytes);
    }
    drop(rows);
    drop(statement);
    let (records, bytes): (i64, Option<i64>) = connection.query_row(
        "SELECT count(*), sum(bytes) FROM stored WHERE content = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    report.content.add(
        u64::try_from(records).unwrap_or(0),
        u64::try_from(bytes.unwrap_or(0)).unwrap_or(0),
    );
    let reached: i64 =
        connection.query_row("SELECT count(*) FROM objects", [], |row| row.get(0))?;
    let listed_and_reached: u64 = report.reachable.values().map(|counts| counts.records).sum();
    report.unlisted = u64::try_from(reached)
        .unwrap_or(0)
        .saturating_sub(listed_and_reached);
    Ok(report)
}
