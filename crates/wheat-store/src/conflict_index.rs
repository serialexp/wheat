//! A local index of each view's unresolved conflicts in canonical order, so a page of
//! a view's conflicts, or the conflict at a position, costs O(log n + page) instead
//! of a walk over every application in the view.
//!
//! A view's unresolved conflicts are its composition parent's plus its last
//! application's conflict, if that conflicted (increment validation refuses any other
//! view), so they are the conflicted applications along the view's application chain,
//! in position order. The index keeps that chain's conflicted applications as a
//! persistent list: an entry per conflicted application, pointing at the entry before
//! it in the chain, and a row per indexed application naming the last entry of its
//! view's list. Views sharing a prefix share its entries, so a resolution, which
//! recomposes from the view before the resolved change, starts its new chain from
//! that view's list and drops the resolved entry without deleting anything.
//!
//! Each entry also holds a jump pointer (Myers' "applicative random access stack"),
//! so finding the entry at an ordinal, or the first at or after a position, costs
//! O(log n) row reads from the list's last entry.
//!
//! No writer knows about the index: a read first brings it up to the view it asks
//! about, walking back from the view's producing application to the first one the
//! index holds and adding each application passed, oldest first, as path history
//! does. An indexed application's predecessors are therefore always indexed, and an
//! entry is a function of its application alone, so two processes catching up at once
//! only write the same rows twice.
//!
//! The views are the authority; the index can be deleted at any time.

use std::{
    num::NonZeroUsize,
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, Row, params};
use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, ConflictId, RevisionId, ViewRecord,
};

use crate::{ImmutableObjectStore, RepositoryPublicationError, RepositoryReader};

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("conflict_index_sql/schema.sql");
const DROP: &str = include_str!("conflict_index_sql/drop.sql");
const APPLICATION_TAIL: &str = include_str!("conflict_index_sql/application_tail.sql");
const INSERT_APPLICATION: &str = include_str!("conflict_index_sql/insert_application.sql");
const INSERT_ENTRY: &str = include_str!("conflict_index_sql/insert_entry.sql");
const ENTRY_BY_APPLICATION: &str = include_str!("conflict_index_sql/entry_by_application.sql");
const ENTRY: &str = include_str!("conflict_index_sql/entry.sql");

/// How long a write waits for another process's write to finish.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many applications catching up writes in one transaction.
const CHUNK_APPLICATIONS: usize = 1024;

#[derive(Debug, Error)]
pub enum ConflictIndexError {
    #[error("conflict index file failed")]
    File(#[from] std::io::Error),
    #[error("conflict index database failed")]
    Database(#[from] rusqlite::Error),
    #[error("conflict index holds a malformed row")]
    Malformed,
    #[error(transparent)]
    Read(#[from] RepositoryPublicationError),
}

/// One unresolved conflict in a view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedConflict {
    /// The conflicted change's index in the view's canonical order.
    pub position: u64,
    pub conflict: ConflictId,
    /// The view's application of the change, whose result is the conflict.
    pub application: ApplicationId,
    pub change: ChangeId,
}

/// A page of [`ConflictIndex::conflicts`], in canonical order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexedConflicts {
    pub conflicts: Vec<IndexedConflict>,
    /// The `after` for the next page; none after the last conflict.
    pub next: Option<u64>,
}

/// An entry of the list: a conflicted application, its place in its chain's list,
/// and the entries it points at.
#[derive(Clone, Copy, Debug)]
struct Entry {
    id: i64,
    previous: Option<i64>,
    /// The entry the jump pointer names; none on the first entry, whose jump is
    /// itself.
    jump: Option<i64>,
    jump_ordinal: u64,
    jump_position: u64,
    /// The entry's index in its list.
    ordinal: u64,
    conflict: IndexedConflict,
}

impl Entry {
    fn from_row(row: &Row<'_>) -> Result<Self, ConflictIndexError> {
        let unsigned = |value: i64| u64::try_from(value).map_err(|_| ConflictIndexError::Malformed);
        let digest = |index| -> Result<[u8; 32], ConflictIndexError> {
            row.get::<_, Vec<u8>>(index)?
                .try_into()
                .map_err(|_| ConflictIndexError::Malformed)
        };
        let change: [u8; 16] = row
            .get::<_, Vec<u8>>(9)?
            .try_into()
            .map_err(|_| ConflictIndexError::Malformed)?;
        Ok(Self {
            id: row.get(0)?,
            previous: row.get(1)?,
            jump: row.get(2)?,
            jump_ordinal: unsigned(row.get(3)?)?,
            jump_position: unsigned(row.get(4)?)?,
            ordinal: unsigned(row.get(5)?)?,
            conflict: IndexedConflict {
                position: unsigned(row.get(6)?)?,
                conflict: ConflictId::from_digest(digest(7)?),
                application: ApplicationId::from_digest(digest(8)?),
                change: ChangeId::from_bytes(change),
            },
        })
    }

    fn position(&self) -> u64 {
        self.conflict.position
    }
}

/// An application catching up adds: its conflict, when it has one.
struct Pending {
    application: ApplicationId,
    conflict: Option<(u64, ConflictId, RevisionId)>,
}

/// Each view's unresolved conflicts, kept in a SQLite file next to the store. See
/// the module documentation.
///
/// The connection sits behind a mutex so a read future holding `&self` across an
/// await is `Send`; it is never held across one.
pub struct ConflictIndex {
    connection: Mutex<Connection>,
}

impl ConflictIndex {
    /// Opens the index at `path`, creating it when missing. A file this version cannot
    /// read is emptied: the index holds nothing the views do not.
    pub fn open(path: &Path) -> Result<Self, ConflictIndexError> {
        let connection = match Self::connect(path) {
            Ok(connection) => connection,
            Err(ConflictIndexError::Database(_)) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut file = path.as_os_str().to_owned();
                    file.push(suffix);
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Self::connect(path)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connect(path: &Path) -> Result<Connection, ConflictIndexError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            connection.execute_batch(DROP)?;
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(connection)
    }

    /// The connection. A panic mid-statement leaves an open transaction that rolls
    /// back, so a poisoned lock is taken as is.
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Indexes the applications in `view`'s history the index does not hold yet and
    /// returns how many. Costs an application read for each, a revision read for each
    /// that conflicted, and one more application read to find where to stop.
    #[tracing::instrument(name = "conflict_index_catch_up", level = "debug", skip_all)]
    pub async fn catch_up<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
    ) -> Result<usize, ConflictIndexError> {
        let mut pending = Vec::new();
        let mut next = reader.producing_application(view).await?;
        let mut tail = None;
        while let Some(id) = next {
            if let Some(indexed) = self.application_tail(id)? {
                tail = indexed;
                break;
            }
            let record = reader.application(id).await?;
            if record.id() != id {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "application does not match its identity",
                )
                .into());
            }
            next = record.order.previous;
            pending.push(Pending {
                application: id,
                conflict: match record.result {
                    ApplicationResultRecord::Clean { .. } => None,
                    ApplicationResultRecord::Conflicted { conflict, .. } => {
                        Some((record.order.position, conflict, record.revision_id))
                    }
                },
            });
        }
        let count = pending.len();
        let mut tail = match tail {
            Some(id) => Some(self.entry(id)?),
            None => None,
        };
        let mut chunk = Vec::with_capacity(CHUNK_APPLICATIONS.min(count));
        for application in pending.into_iter().rev() {
            let conflict = match application.conflict {
                None => None,
                Some((position, conflict, revision)) => Some(IndexedConflict {
                    position,
                    conflict,
                    application: application.application,
                    change: reader.revision(revision).await?.change_id,
                }),
            };
            chunk.push((application.application, conflict));
            if chunk.len() == CHUNK_APPLICATIONS {
                tail = self.write(tail, &chunk)?;
                chunk.clear();
            }
        }
        self.write(tail, &chunk)?;
        Ok(count)
    }

    /// The last entry of an indexed application's list: `None` when the index does
    /// not hold the application, `Some(None)` when its view has no conflicts.
    fn application_tail(
        &self,
        id: ApplicationId,
    ) -> Result<Option<Option<i64>>, ConflictIndexError> {
        Ok(self
            .connection()
            .prepare_cached(APPLICATION_TAIL)?
            .query_row([id.as_bytes()], |row| row.get(0))
            .optional()?)
    }

    fn entry(&self, id: i64) -> Result<Entry, ConflictIndexError> {
        read_entry(&self.connection(), ENTRY, id)
    }

    /// Writes `chunk`'s applications, oldest first, in one transaction, after `tail`,
    /// the last entry of the first one's predecessor; returns the last entry after
    /// them.
    fn write(
        &self,
        mut tail: Option<Entry>,
        chunk: &[(ApplicationId, Option<IndexedConflict>)],
    ) -> Result<Option<Entry>, ConflictIndexError> {
        if chunk.is_empty() {
            return Ok(tail);
        }
        let mut connection = self.connection();
        let transaction = connection.transaction()?;
        {
            let mut insert_application = transaction.prepare_cached(INSERT_APPLICATION)?;
            let mut insert_entry = transaction.prepare_cached(INSERT_ENTRY)?;
            for (application, conflict) in chunk {
                if let Some(conflict) = conflict {
                    let (jump, jump_ordinal, jump_position) = match &tail {
                        None => (None, 0, conflict.position),
                        Some(parent) => {
                            let next = jump_after(&transaction, parent)?;
                            (Some(next.0), next.1, next.2)
                        }
                    };
                    let ordinal = tail.as_ref().map_or(0, |parent| parent.ordinal + 1);
                    insert_entry.execute(params![
                        application.as_bytes(),
                        tail.as_ref().map(|parent| parent.id),
                        jump,
                        signed(jump_ordinal)?,
                        signed(jump_position)?,
                        signed(ordinal)?,
                        signed(conflict.position)?,
                        conflict.conflict.as_bytes(),
                        conflict.change.as_bytes(),
                    ])?;
                    // Another process may have written the same entry first; it is the
                    // same entry, so read whichever is stored.
                    let entry = read_entry_by_application(&transaction, *application)?;
                    if entry.ordinal != ordinal || entry.conflict != *conflict {
                        return Err(ConflictIndexError::Malformed);
                    }
                    tail = Some(entry);
                }
                insert_application.execute(params![
                    application.as_bytes(),
                    tail.as_ref().map(|entry| entry.id)
                ])?;
            }
        }
        transaction.commit()?;
        Ok(tail)
    }

    /// The last entry of `view`'s list, catching the index up to `view` first.
    async fn tail<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
    ) -> Result<Option<Entry>, ConflictIndexError> {
        self.catch_up(reader, view).await?;
        let Some(producing) = reader.producing_application(view).await? else {
            return Ok(None);
        };
        match self.application_tail(producing)? {
            Some(Some(id)) => Ok(Some(self.entry(id)?)),
            Some(None) => Ok(None),
            None => Err(ConflictIndexError::Malformed),
        }
    }

    /// How many unresolved conflicts `view` has. Once the index holds `view`, costs an
    /// application map lookup and two row reads.
    pub async fn count<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
    ) -> Result<u64, ConflictIndexError> {
        Ok(self
            .tail(reader, view)
            .await?
            .map_or(0, |tail| tail.ordinal + 1))
    }

    /// Up to `limit` of `view`'s unresolved conflicts after canonical position `after`
    /// (from the first when `None`), earliest first. Catches the index up to `view`
    /// first; then costs O(log n) row reads to find the page and one per conflict on
    /// it.
    #[tracing::instrument(name = "conflict_index_conflicts", level = "debug", skip_all)]
    pub async fn conflicts<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
        after: Option<u64>,
        limit: NonZeroUsize,
    ) -> Result<IndexedConflicts, ConflictIndexError> {
        let Some(tail) = self.tail(reader, view).await? else {
            return Ok(IndexedConflicts::default());
        };
        let from = match after {
            None => 0,
            Some(after) => match after.checked_add(1) {
                Some(from) => from,
                None => return Ok(IndexedConflicts::default()),
            },
        };
        let connection = self.connection();
        let read = |id| read_entry(&connection, ENTRY, id);
        let Some(first) = first_from(tail, from, read)? else {
            return Ok(IndexedConflicts::default());
        };
        let end = first
            .ordinal
            .saturating_add(limit.get() as u64 - 1)
            .min(tail.ordinal);
        let last = ancestor(tail, end, read)?;
        let mut conflicts = Vec::with_capacity((end - first.ordinal + 1) as usize);
        let mut entry = last;
        loop {
            conflicts.push(entry.conflict);
            if entry.ordinal == first.ordinal {
                break;
            }
            let previous = entry.previous.ok_or(ConflictIndexError::Malformed)?;
            entry = read(previous)?;
        }
        conflicts.reverse();
        Ok(IndexedConflicts {
            conflicts,
            next: (end < tail.ordinal).then(|| last.position()),
        })
    }

    /// `view`'s unresolved conflict at canonical position `position`, if the change
    /// there conflicted. Catches the index up to `view` first; then costs O(log n) row
    /// reads.
    pub async fn at<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
        position: u64,
    ) -> Result<Option<IndexedConflict>, ConflictIndexError> {
        let Some(tail) = self.tail(reader, view).await? else {
            return Ok(None);
        };
        let connection = self.connection();
        Ok(
            first_from(tail, position, |id| read_entry(&connection, ENTRY, id))?
                .filter(|entry| entry.position() == position)
                .map(|entry| entry.conflict),
        )
    }
}

fn signed(value: u64) -> Result<i64, ConflictIndexError> {
    i64::try_from(value).map_err(|_| ConflictIndexError::Malformed)
}

fn read_entry(connection: &Connection, sql: &str, id: i64) -> Result<Entry, ConflictIndexError> {
    connection
        .prepare_cached(sql)?
        .query_row([id], |row| Ok(Entry::from_row(row)))?
}

fn read_entry_by_application(
    connection: &Connection,
    application: ApplicationId,
) -> Result<Entry, ConflictIndexError> {
    connection
        .prepare_cached(ENTRY_BY_APPLICATION)?
        .query_row([application.as_bytes()], |row| Ok(Entry::from_row(row)))?
}

/// The jump pointer of an entry appended after `parent`: its ID, ordinal and
/// position. When `parent`'s jump spans as many entries as its jump's jump, the new
/// entry jumps over both; otherwise it jumps to `parent`. That keeps every entry's
/// jump distance a skew-binary number, so a search makes O(log n) steps.
fn jump_after(
    connection: &Connection,
    parent: &Entry,
) -> Result<(i64, u64, u64), ConflictIndexError> {
    let Some(jump) = parent.jump else {
        // The parent is the first entry, whose jump is itself.
        return Ok((parent.id, parent.ordinal, parent.position()));
    };
    let target = read_entry(connection, ENTRY, jump)?;
    let (beyond, beyond_ordinal, beyond_position) = match target.jump {
        Some(beyond) => (beyond, target.jump_ordinal, target.jump_position),
        None => (target.id, target.ordinal, target.position()),
    };
    if parent.ordinal - target.ordinal == target.ordinal - beyond_ordinal {
        Ok((beyond, beyond_ordinal, beyond_position))
    } else {
        Ok((parent.id, parent.ordinal, parent.position()))
    }
}

/// The entry at `ordinal` in the list ending at `entry`, which is at or after it.
fn ancestor(
    mut entry: Entry,
    ordinal: u64,
    mut read: impl FnMut(i64) -> Result<Entry, ConflictIndexError>,
) -> Result<Entry, ConflictIndexError> {
    while entry.ordinal > ordinal {
        let next = match entry.jump {
            Some(jump) if entry.jump_ordinal >= ordinal => jump,
            _ => entry.previous.ok_or(ConflictIndexError::Malformed)?,
        };
        entry = read(next)?;
    }
    Ok(entry)
}

/// The earliest entry at or after canonical position `position` in the list ending
/// at `entry`, if any. Positions grow along the list.
fn first_from(
    mut entry: Entry,
    position: u64,
    mut read: impl FnMut(i64) -> Result<Entry, ConflictIndexError>,
) -> Result<Option<Entry>, ConflictIndexError> {
    if entry.position() < position {
        return Ok(None);
    }
    loop {
        let Some(previous) = entry.previous else {
            return Ok(Some(entry));
        };
        if let Some(jump) = entry.jump
            && entry.jump_position >= position
        {
            entry = read(jump)?;
            continue;
        }
        let before = read(previous)?;
        if before.position() < position {
            return Ok(Some(entry));
        }
        entry = before;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// An index holding one list of `count` entries at positions 3, 6, 9, ...; returns
    /// it and the list's last entry.
    fn list(count: u64) -> (tempfile::TempDir, ConflictIndex, Entry) {
        let directory = tempfile::tempdir().unwrap();
        let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
        let digest = |ordinal: u64, kind: u8| {
            let mut bytes = [kind; 32];
            bytes[..8].copy_from_slice(&ordinal.to_be_bytes());
            bytes
        };
        let chunk = (0..count)
            .map(|ordinal| {
                let application = ApplicationId::from_digest(digest(ordinal, 1));
                (
                    application,
                    Some(IndexedConflict {
                        position: 3 * (ordinal + 1),
                        conflict: ConflictId::from_digest(digest(ordinal, 2)),
                        application,
                        change: ChangeId::from_bytes([3; 16]),
                    }),
                )
            })
            .collect::<Vec<_>>();
        let tail = index.write(None, &chunk).unwrap().unwrap();
        (directory, index, tail)
    }

    #[test]
    fn a_seek_finds_every_entry_in_logarithmically_many_row_reads() {
        let count = 5_000;
        let (_directory, index, tail) = list(count);
        let connection = index.connection();
        let reads = Cell::new(0_u64);
        let read = |id| {
            reads.set(reads.get() + 1);
            read_entry(&connection, ENTRY, id)
        };
        // Myers' bound is about 3 log2 n steps; allow a little over.
        let bound = 3 * u64::from(count.ilog2() + 1) + 4;
        let mut most = 0;
        for ordinal in 0..count {
            reads.set(0);
            assert_eq!(ancestor(tail, ordinal, read).unwrap().ordinal, ordinal);
            most = most.max(reads.get());
            for position in [3 * ordinal + 1, 3 * ordinal + 3] {
                reads.set(0);
                let found = first_from(tail, position, read).unwrap().unwrap();
                assert_eq!(found.ordinal, ordinal, "from position {position}");
                most = most.max(reads.get());
            }
        }
        assert!(first_from(tail, 3 * count + 1, read).unwrap().is_none());
        assert!(most <= bound, "a seek read {most} rows, over {bound}");
    }
}
