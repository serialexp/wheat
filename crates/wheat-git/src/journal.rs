//! Durable, local scratch state for a bounded Git import.
//!
//! This journal is resumability state, not authority for the immutable objects it names.
//! Callers must durably write and verify those objects before calling [`ImportJournal::complete`].

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;
use wheat_core::{ApplicationId, ChangeId, RevisionId, ViewId};

const FORMAT_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("import journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("import journal SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("journal belongs to a different source snapshot or format version")]
    SnapshotMismatch,
    #[error("journal is already owned by another importer: {0}")]
    AlreadyOwned(PathBuf),
    #[error("work item is not present in the journal")]
    UnknownWork,
    #[error("work item has incomplete parents")]
    IncompleteParents,
    #[error("work item was already completed with different native IDs")]
    ConflictingCompletion,
    #[error("journal contains an invalid {0}")]
    Corrupt(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingWork {
    pub oid: Vec<u8>,
    /// Source parent OIDs in their original order (including non-first parents).
    pub parents: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeResult {
    pub change_id: ChangeId,
    pub revision_id: RevisionId,
    pub view_id: ViewId,
    pub application_id: ApplicationId,
}

/// An exclusively-owned journal. Dropping it releases the process lock.
pub struct ImportJournal {
    connection: Connection,
    _ownership: File,
}

impl ImportJournal {
    /// Opens or creates a journal bound to exactly the caller's canonical snapshot bytes.
    pub fn open(path: impl AsRef<Path>, source_snapshot: &[u8]) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        if unsafe { libc::flock(ownership.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(JournalError::AlreadyOwned(lock_path));
            }
            return Err(error.into());
        }

        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(include_str!("journal/schema.sql"))?;

        let identity = connection
            .query_row(
                "SELECT format_version, source_snapshot FROM journal_identity WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        match identity {
            None => {
                connection.execute(
                    "INSERT INTO journal_identity(singleton, format_version, source_snapshot) VALUES (1, ?1, ?2)",
                    params![FORMAT_VERSION, source_snapshot],
                )?;
            }
            Some((version, snapshot))
                if version == FORMAT_VERSION && snapshot == source_snapshot => {}
            Some(_) => return Err(JournalError::SnapshotMismatch),
        }

        Ok(Self {
            connection,
            _ownership: ownership,
        })
    }

    /// Persistently adds source commits. Re-adding an identical item is idempotent.
    pub fn add_pending(&mut self, oid: &[u8], parents: &[Vec<u8>]) -> Result<(), JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<i64> = transaction
            .query_row("SELECT 1 FROM work WHERE oid = ?1", [oid], |row| row.get(0))
            .optional()?;
        if existing.is_none() {
            transaction.execute("INSERT INTO work(oid, status) VALUES (?1, 0)", [oid])?;
            for (position, parent) in parents.iter().enumerate() {
                transaction.execute(
                    "INSERT INTO work_parent(oid, position, parent_oid) VALUES (?1, ?2, ?3)",
                    params![oid, position as i64, parent],
                )?;
            }
        } else {
            let actual = parents_for(&transaction, oid)?;
            if actual != parents {
                return Err(JournalError::Corrupt("parent list for duplicate work item"));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns at most `limit` pending commits whose every parent is complete.
    /// Ordering is stable insertion order, while parent order is retained separately.
    pub fn ready(&self, limit: usize) -> Result<Vec<PendingWork>, JournalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT w.oid FROM work w WHERE w.status = 0 AND NOT EXISTS (\
             SELECT 1 FROM work_parent p LEFT JOIN work pw ON pw.oid = p.parent_oid \
             WHERE p.oid = w.oid AND (pw.oid IS NULL OR pw.status != 1)) \
             ORDER BY w.sequence LIMIT ?1",
        )?;
        let oids = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| row.get(0))?
            .collect::<Result<Vec<Vec<u8>>, _>>()?;
        oids.into_iter()
            .map(|oid| {
                Ok(PendingWork {
                    parents: parents_for(&self.connection, &oid)?,
                    oid,
                })
            })
            .collect()
    }

    pub fn result(&self, oid: &[u8]) -> Result<Option<NativeResult>, JournalError> {
        self.connection
            .query_row(
                "SELECT change_id, revision_id, view_id, application_id FROM work WHERE oid = ?1 AND status = 1",
                [oid],
                |row| decode_result(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?),
            )
            .optional()
            .map_err(JournalError::from)
    }

    /// Returns the source parent OIDs in their original order.
    pub fn parents(&self, oid: &[u8]) -> Result<Vec<Vec<u8>>, JournalError> {
        let exists = self
            .connection
            .query_row("SELECT 1 FROM work WHERE oid = ?1", [oid], |_| Ok(()))
            .optional()?;
        if exists.is_none() {
            return Err(JournalError::UnknownWork);
        }
        Ok(parents_for(&self.connection, oid)?)
    }

    /// Returns a bounded set of parent OIDs referenced by work but not yet discovered.
    pub fn missing_parents(&self, limit: usize) -> Result<Vec<Vec<u8>>, JournalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare(include_str!("journal/missing_parents.sql"))?;
        Ok(statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| row.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Number of completed and pending work rows, respectively.
    pub fn progress(&self) -> Result<(usize, usize), JournalError> {
        let (complete, pending): (i64, i64) =
            self.connection
                .query_row(include_str!("journal/progress.sql"), [], |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                })?;
        Ok((
            usize::try_from(complete).map_err(|_| JournalError::Corrupt("completed count"))?,
            usize::try_from(pending).map_err(|_| JournalError::Corrupt("pending count"))?,
        ))
    }

    /// Bounded keyset page of completed mappings, ordered by source OID.
    pub fn completed(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, NativeResult)>, JournalError> {
        let mut statement = self
            .connection
            .prepare(include_str!("journal/completed.sql"))?;
        Ok(statement
            .query_map(
                params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok((
                        row.get(0)?,
                        decode_result(row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)?,
                    ))
                },
            )?
            .collect::<Result<_, _>>()?)
    }

    /// Atomically checkpoints completion after the caller has written immutable objects.
    /// Repeating the same result is idempotent; a different result is rejected.
    pub fn complete(&mut self, oid: &[u8], result: NativeResult) -> Result<(), JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<i64> = transaction
            .query_row("SELECT status FROM work WHERE oid = ?1", [oid], |row| {
                row.get(0)
            })
            .optional()?;
        match status {
            None => return Err(JournalError::UnknownWork),
            Some(1) => {
                let actual = transaction.query_row(
                    "SELECT change_id, revision_id, view_id, application_id FROM work WHERE oid = ?1",
                    [oid],
                    |row| decode_result(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?),
                )?;
                if actual != result {
                    return Err(JournalError::ConflictingCompletion);
                }
            }
            Some(0) => {
                let incomplete: bool = transaction.query_row(
                    include_str!("journal/incomplete_parents.sql"),
                    [oid],
                    |row| row.get(0),
                )?;
                if incomplete {
                    return Err(JournalError::IncompleteParents);
                }
                transaction.execute(
                    "UPDATE work SET status = 1, change_id = ?2, revision_id = ?3, view_id = ?4, application_id = ?5 WHERE oid = ?1",
                    params![oid, result.change_id.as_bytes().as_slice(), result.revision_id.as_bytes().as_slice(), result.view_id.as_bytes().as_slice(), result.application_id.as_bytes().as_slice()],
                )?;
            }
            Some(_) => return Err(JournalError::Corrupt("work status")),
        }
        transaction.commit()?;
        Ok(())
    }
}

fn parents_for(connection: &Connection, oid: &[u8]) -> Result<Vec<Vec<u8>>, rusqlite::Error> {
    let mut statement = connection
        .prepare("SELECT parent_oid FROM work_parent WHERE oid = ?1 ORDER BY position")?;
    statement.query_map([oid], |row| row.get(0))?.collect()
}

fn decode_result(
    change: Vec<u8>,
    revision: Vec<u8>,
    view: Vec<u8>,
    application: Vec<u8>,
) -> Result<NativeResult, rusqlite::Error> {
    Ok(NativeResult {
        change_id: ChangeId::from_bytes(array(change, "change ID")?),
        revision_id: RevisionId::from_digest(array(revision, "revision ID")?),
        view_id: ViewId::from_digest(array(view, "view ID")?),
        application_id: ApplicationId::from_digest(array(application, "application ID")?),
    })
}

fn array<const N: usize>(bytes: Vec<u8>, name: &'static str) -> Result<[u8; N], rusqlite::Error> {
    bytes.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(io::Error::new(io::ErrorKind::InvalidData, name)),
        )
    })
}
