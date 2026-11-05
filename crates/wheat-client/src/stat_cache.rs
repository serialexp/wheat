//! A workspace's stat cache: for each tracked path, the stat its leaf had when status
//! last read it and what the read found, so status reads only the files whose stat
//! changed. It lives in `.wheat/stat.sqlite`; it is a cache, emptied when unreadable.
//!
//! An entry is kept only when its leaf's mtime is older than a timestamp the
//! filesystem gave at the start of the read (Git's "racily clean" problem): a write
//! after the leaf's stat was taken then gets a later mtime, so it changes the stat
//! even when it keeps the length. A leaf written in the same instant as the read is
//! read again next time.
//!
//! A read loads every entry in one scan: status looks up each tracked path, and one
//! query per path costs more than the scan.

use std::{
    collections::{BTreeSet, HashMap},
    io::Write as _,
    path::Path,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, Row, params};
use wheat_core::{ContentId, FileKind};

use crate::workspace_files::{FileSamples, FileStamp};

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("stat_cache_sql/schema.sql");
const DROP: &str = include_str!("stat_cache_sql/drop.sql");
const ALL: &str = include_str!("stat_cache_sql/all.sql");
const PUT: &str = include_str!("stat_cache_sql/put.sql");
const DELETE: &str = include_str!("stat_cache_sql/delete.sql");

/// The cache's file in a workspace's control directory.
pub(crate) const STAT_CACHE_FILE: &str = "stat.sqlite";
/// The file whose mtime gives the filesystem's time at the start of a read.
const CLOCK_FILE: &str = "stat.clock";
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What status found in a leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Observed {
    pub(crate) kind: FileKind,
    /// The canonical content's ID.
    pub(crate) content: ContentId,
    /// The canonical content's length.
    pub(crate) bytes: usize,
    pub(crate) text: bool,
    pub(crate) samples: Option<FileSamples>,
}

/// A workspace's stat cache, open for one read of the workspace. Entries recorded are
/// written by [`StatCache::finish`].
pub(crate) struct StatCache {
    connection: Connection,
    /// The filesystem's time when the read started, in nanoseconds.
    clock_ns: i64,
    entries: HashMap<String, (FileStamp, Observed)>,
    pending: Vec<(String, FileStamp, Observed)>,
}

impl StatCache {
    /// Opens the cache of the workspace at `root`, creating it when missing, loads its
    /// entries, and takes the filesystem's time for the read about to start.
    #[tracing::instrument(name = "stat_cache_open", level = "debug", skip_all)]
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let control = root.join(".wheat");
        let path = control.join(STAT_CACHE_FILE);
        let opened = connect(&path).and_then(|connection| {
            let entries = load(&connection)?;
            Ok((connection, entries))
        });
        let (connection, entries) = match opened {
            Ok(opened) => opened,
            Err(_) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut file = path.as_os_str().to_owned();
                    file.push(suffix);
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error).context("cannot reset the stat cache"),
                    }
                }
                let connection = connect(&path).context("cannot open the stat cache")?;
                (connection, HashMap::new())
            }
        };
        Ok(Self {
            connection,
            clock_ns: filesystem_clock(&control.join(CLOCK_FILE))?,
            entries,
            pending: Vec::new(),
        })
    }

    /// The stamp `path` had when it was recorded and what it held then.
    pub(crate) fn get(&self, path: &str) -> Option<&(FileStamp, Observed)> {
        self.entries.get(path)
    }

    /// Records that a leaf with `stamp` at `path` holds `observed`, unless its mtime
    /// is not older than the read's start.
    pub(crate) fn record(&mut self, path: &str, stamp: FileStamp, observed: Observed) {
        if stamp.mtime_ns < self.clock_ns {
            self.pending.push((path.to_owned(), stamp, observed));
        }
    }

    /// Writes the entries recorded. With `tracked`, every tracked path, entries for
    /// other paths are dropped once they outnumber the tracked ones.
    #[tracing::instrument(name = "stat_cache_finish", level = "debug", skip_all)]
    pub(crate) fn finish(mut self, tracked: Option<&BTreeSet<String>>) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut put = transaction.prepare_cached(PUT)?;
            for (path, stamp, observed) in &self.pending {
                put.execute(params![
                    path,
                    stamp.size as i64,
                    stamp.mtime_ns,
                    stamp.ctime_ns,
                    stamp.inode as i64,
                    stamp.device as i64,
                    stamp.mode,
                    kind_code(observed.kind),
                    observed.content.as_bytes(),
                    observed.bytes as i64,
                    observed.text,
                    observed.samples.as_ref().map(|samples| samples.as_slice()),
                ])?;
            }
            if let Some(tracked) = tracked
                && self.entries.len() > 2 * tracked.len() + 1024
            {
                let mut delete = transaction.prepare(DELETE)?;
                for path in self.entries.keys() {
                    if !tracked.contains(path) {
                        delete.execute([path])?;
                    }
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Every entry in the cache. A row this version cannot read is left out, as good as
/// no row.
fn load(connection: &Connection) -> rusqlite::Result<HashMap<String, (FileStamp, Observed)>> {
    let mut statement = connection.prepare(ALL)?;
    let mut rows = statement.query([])?;
    let mut entries = HashMap::new();
    while let Some(row) = rows.next()? {
        if let Some(entry) = entry(row)? {
            entries.insert(row.get(0)?, entry);
        }
    }
    Ok(entries)
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Option<(FileStamp, Observed)>> {
    let stamp = FileStamp {
        size: row.get::<_, i64>(1)? as u64,
        mtime_ns: row.get(2)?,
        ctime_ns: row.get(3)?,
        inode: row.get::<_, i64>(4)? as u64,
        device: row.get::<_, i64>(5)? as u64,
        mode: row.get(6)?,
    };
    let samples = match row.get_ref(11)?.as_blob_or_null()? {
        None => Ok(None),
        Some(samples) => <[u8; 32]>::try_from(samples).map(Some),
    };
    let (Some(kind), Ok(content), Ok(bytes), Ok(samples)) = (
        kind_of(row.get(7)?),
        <[u8; 32]>::try_from(row.get_ref(8)?.as_blob()?),
        usize::try_from(row.get::<_, i64>(9)?),
        samples,
    ) else {
        return Ok(None);
    };
    Ok(Some((
        stamp,
        Observed {
            kind,
            content: ContentId::from_digest(content),
            bytes,
            text: row.get(10)?,
            samples,
        },
    )))
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
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

/// The filesystem's time now, in nanoseconds: the mtime a write to `path` gets. It
/// comes from the same clock as the workspace files' mtimes, which the process's
/// clock need not agree with (a network filesystem's server sets them).
fn filesystem_clock(path: &Path) -> Result<i64> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .context("cannot write the stat cache's clock")?;
    file.write_all(b"stat cache clock\n")?;
    let modified = file.metadata()?;
    Ok(mtime_ns(&modified))
}

#[cfg(unix)]
fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec()
}

/// Workspace files are only read on Unix; elsewhere nothing is recorded.
#[cfg(not(unix))]
fn mtime_ns(_metadata: &std::fs::Metadata) -> i64 {
    i64::MIN
}

fn kind_code(kind: FileKind) -> i64 {
    match kind {
        FileKind::Regular => 0,
        FileKind::Executable => 1,
        FileKind::Symlink => 2,
    }
}

fn kind_of(code: i64) -> Option<FileKind> {
    match code {
        0 => Some(FileKind::Regular),
        1 => Some(FileKind::Executable),
        2 => Some(FileKind::Symlink),
        _ => None,
    }
}
