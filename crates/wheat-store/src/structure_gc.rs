//! What a pool holds that no head reaches: the dry run of structural garbage
//! collection (`docs/design/wheat-network-maintenance.md`, "Pool GC").
//!
//! Every publish leaves the previous envelope, its label manifest and the old nodes of
//! each set the new state rewrote unreachable, and nothing removes them. This walks
//! the closure of every head of a network, lists what the store holds, and counts
//! what no head reaches, by kind. It deletes nothing, and so needs no hold on the
//! network: a publish while it runs can leave its new objects counted as
//! unreachable, since the walk read the heads before it. The real collection checks
//! each head completely and holds publishes (Bart, 2026-11-12).

use std::collections::BTreeMap;

use rusqlite::params;
use wheat_core::{ContentId, ObjectId, RepositoryEnvelopeId};

use crate::{
    ImmutableObjectStore, ObjectKind, ObjectStoreError, SnapshotError, SnapshotLimits,
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
pub trait StoredInventory {
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
    let mut walk = ClosureWalk::new(limits)?;
    for root in roots {
        walk.enqueue_object((*root).into())?;
    }
    while let Some(visit) = walk.next_object()? {
        let stored = objects.get(visit.id).await?;
        walk.follow(&stored, &visit)?;
    }

    let connection = walk.connection();
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

    let mut report = StructureReport {
        roots: roots.len(),
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
