//! Content garbage collection over an embedder's blob store, for Gothab: content is
//! shared by every repository of an instance, heads are spread over hosts, and each
//! host holds the structure of the networks it serves. See
//! `docs/design/wheat-network-maintenance.md`, "Gothab's content".
//!
//! Each host marks the heads it serves into a [`ContentMarks`] file and finishes it;
//! a worker [merges](ContentMarks::merge) the hosts' files and [`sweep`]s the blob
//! store, asking the embedder's [`ContentInventory`] what each record refers to and
//! to reclaim what nothing marked. Marking and sweeping fail closed: anything they
//! cannot read stops them, and a mark file not finished is never swept against.
//!
//! What a publish names while a collection runs is the embedder's to keep: the sweep
//! only asks it to reclaim, and the embedder declines content named since the mark
//! began.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use wheat_core::{ContentId, RepositoryEnvelopeId, StoreRecordError, decode_content_chunk_part};

use crate::{
    CollectionError, ContentBlobStore, ContentStore, ContentStoreError, ImmutableObjectStore,
    RecordReferences, RepositoryPublicationError, RepositoryReader,
    content_closure::MAX_PART_BYTES,
    radix::{Key as _, Store as RadixStore},
    sets::ContentCollection,
};

/// Content IDs written to the mark file at once.
const INSERT_BATCH: usize = 4096;
/// Marked IDs whose references are asked for at once.
const REFERENCE_BATCH: usize = 1024;
/// IDs asked for per listed page, and so the most one [`ContentInventory::reclaim`]
/// is given.
pub const SWEEP_PAGE_IDS: usize = 1000;

const SCHEMA_VERSION: i64 = 2;
const SCHEMA: &str = "
CREATE TABLE marked (
    id BLOB PRIMARY KEY NOT NULL,
    expanded INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX unexpanded ON marked (id) WHERE expanded = 0;
CREATE TABLE visited (node BLOB PRIMARY KEY NOT NULL) WITHOUT ROWID;
CREATE TABLE started (one INTEGER PRIMARY KEY CHECK (one = 1), at_ms INTEGER NOT NULL);
CREATE TABLE finished (one INTEGER PRIMARY KEY CHECK (one = 1), marked INTEGER NOT NULL);
";

/// Why a collection stopped. Nothing is reclaimed after an error.
#[derive(Debug, Error)]
pub enum GcError {
    #[error("mark file: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("mark file: {0}")]
    File(String),
    #[error("reading a head: {0}")]
    Head(#[from] RepositoryPublicationError),
    #[error("reading a content set: {0}")]
    ContentSet(#[from] CollectionError),
    #[error("reading a long blob's part: {0}")]
    Content(#[from] ContentStoreError),
    #[error("part {0} is not a list of chunks: {1}")]
    Part(ContentId, StoreRecordError),
    #[error("the content inventory: {0}")]
    Inventory(#[from] io::Error),
    #[error("the content inventory has no record of marked content {0}")]
    Unknown(ContentId),
    #[error("the blob store does not list what it holds")]
    NotListed,
}

/// What marking read.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarkCounts {
    pub heads: u64,
    /// Content set nodes read, and those skipped as already marked from another head.
    pub nodes_read: u64,
    pub nodes_skipped: u64,
    /// Content IDs newly marked.
    pub marked: u64,
}

/// What a sweep did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepCounts {
    /// Marked IDs whose references were followed, in this many inventory calls.
    pub followed: u64,
    pub reference_calls: u64,
    /// Content marked because a marked record refers to it.
    pub referenced: u64,
    /// Long blobs' parts read for their chunks.
    pub parts_read: u64,
    /// IDs the blob store listed, in this many pages; of those marked by nothing,
    /// the ones the inventory reclaimed and the ones it kept (named since the mark
    /// began), in this many inventory calls, at most one a page.
    pub listed: u64,
    pub pages: u64,
    pub reclaimed: u64,
    pub kept: u64,
    pub reclaim_calls: u64,
}

/// The embedder's record of its blob store, for [`sweep`].
#[async_trait]
pub trait ContentInventory: Send + Sync {
    /// What each of `ids` refers to, as [`crate::record_info`] gave it when the record
    /// was put. Every ID must be answered; one the embedder has no record of stops the
    /// sweep.
    async fn references(&self, ids: &[ContentId])
    -> io::Result<Vec<(ContentId, RecordReferences)>>;

    /// `ids`, at most [`SWEEP_PAGE_IDS`] of one listed page, are marked by no head:
    /// delete each, unless it was named since the mark began. Answers those deleted.
    async fn reclaim(&self, ids: &[ContentId]) -> io::Result<Vec<ContentId>>;
}

/// A disk-backed set of marked content IDs (SQLite), with the content set nodes
/// already walked, so heads sharing history are read once.
pub struct ContentMarks {
    connection: Connection,
    path: PathBuf,
    finished: bool,
    started: SystemTime,
}

impl std::fmt::Debug for ContentMarks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentMarks")
            .field("path", &self.path)
            .field("finished", &self.finished)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl ContentMarks {
    /// A new, empty mark file at `path`, which must not exist, for the collection
    /// that began at `started`: the heads marked into it must be read after then.
    /// The file records it, to the millisecond, so a worker can refuse a file of an
    /// earlier collection ([`Self::started`]).
    pub fn create(path: &Path, started: SystemTime) -> Result<Self, GcError> {
        if path.exists() {
            return Err(GcError::File(format!("{} already exists", path.display())));
        }
        let at_ms = unix_ms(started)?;
        let connection = Connection::open(path)?;
        // Marks need not survive a crash: a file is used only once finished, and
        // `seal` makes everything durable before it records that.
        connection.pragma_update(None, "journal_mode", "MEMORY")?;
        connection.pragma_update(None, "synchronous", "OFF")?;
        connection.execute_batch(SCHEMA)?;
        connection.execute(
            "INSERT INTO started (one, at_ms) VALUES (1, ?1)",
            params![at_ms],
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            connection,
            path: path.to_owned(),
            finished: false,
            started: from_unix_ms(at_ms),
        })
    }

    /// A finished mark file, to sweep against.
    pub fn open(path: &Path) -> Result<Self, GcError> {
        let (connection, started) = Self::open_finished(path)?;
        Ok(Self {
            connection,
            path: path.to_owned(),
            finished: true,
            started,
        })
    }

    /// When the collection this file marks for began, as [`Self::create`] was told.
    #[must_use]
    pub fn started(&self) -> SystemTime {
        self.started
    }

    fn open_finished(path: &Path) -> Result<(Connection, SystemTime), GcError> {
        if !path.is_file() {
            return Err(GcError::File(format!("{} does not exist", path.display())));
        }
        let connection = Connection::open(path)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(GcError::File(format!(
                "{} is not a mark file of this version",
                path.display()
            )));
        }
        let finished: Option<i64> = connection
            .query_row("SELECT marked FROM finished", [], |row| row.get(0))
            .optional()?;
        if finished.is_none() {
            return Err(GcError::File(format!(
                "{} was not finished, so it may lack marks",
                path.display()
            )));
        }
        let at_ms: i64 = connection.query_row("SELECT at_ms FROM started", [], |row| row.get(0))?;
        Ok((connection, from_unix_ms(at_ms)))
    }

    /// The union of finished mark files at `inputs`, as a new finished file at `path`
    /// for the collection that began at `started`. An input marked for a collection
    /// that began earlier is refused: its heads may have been read before `started`.
    pub fn merge(path: &Path, started: SystemTime, inputs: &[&Path]) -> Result<Self, GcError> {
        let mut marks = Self::create(path, started)?;
        for input in inputs {
            // Opened first, so an unfinished or earlier input is refused before it is
            // read.
            let (_, input_started) = Self::open_finished(input)?;
            if input_started < marks.started {
                return Err(GcError::File(format!(
                    "{} was marked for a collection that began before this one",
                    input.display()
                )));
            }
            let name = input
                .to_str()
                .ok_or_else(|| GcError::File(format!("{} is not UTF-8", input.display())))?;
            marks
                .connection
                .execute("ATTACH DATABASE ?1 AS input", params![name])?;
            let merged = marks.connection.execute(
                "INSERT OR IGNORE INTO marked (id) SELECT id FROM input.marked",
                [],
            );
            marks.connection.execute("DETACH DATABASE input", [])?;
            merged?;
        }
        marks.connection.execute("DELETE FROM visited", [])?;
        marks.seal()?;
        marks.finished = true;
        Ok(marks)
    }

    /// Marks the content each of `envelopes` needs: its state's `required_content`,
    /// skipping content set nodes this file already walked. Every head is read in
    /// full or the call fails.
    #[tracing::instrument(name = "content_gc_mark", level = "debug", skip_all)]
    pub async fn mark_heads<O: ImmutableObjectStore>(
        &mut self,
        reader: &RepositoryReader<'_, O>,
        envelopes: impl IntoIterator<Item = RepositoryEnvelopeId>,
    ) -> Result<MarkCounts, GcError> {
        if self.finished {
            return Err(GcError::File(format!(
                "{} is finished and takes no more marks",
                self.path.display()
            )));
        }
        let store = RadixStore::<_, ContentCollection>::new(reader.objects);
        let mut counts = MarkCounts::default();
        let mut pending = Vec::with_capacity(INSERT_BATCH);
        let mut stack: Vec<Visit> = Vec::new();
        for id in envelopes {
            let envelope = reader.envelope(id).await?;
            let state = reader.state(envelope.state).await?;
            counts.heads += 1;
            let root = Visit {
                id: state.required_content,
                path: Path32::EMPTY,
                leaving: false,
            };
            if self.visited(root.id.as_bytes(), root.path.bytes())? {
                counts.nodes_skipped += 1;
                continue;
            }
            stack.push(root);
            while let Some(visit) = stack.pop() {
                if visit.leaving {
                    // A node counts as walked only once everything below it is
                    // stored, so a later head skips it only then.
                    counts.marked += self.insert(&pending)?;
                    pending.clear();
                    self.record_visited(visit.id.as_bytes(), visit.path.bytes())?;
                    continue;
                }
                let node = if visit.path.len == 0 {
                    store.root(visit.id).await
                } else {
                    store.read_at(visit.id, visit.path.len()).await
                }
                .map_err(CollectionError::from)?;
                counts.nodes_read += 1;
                stack.push(Visit {
                    leaving: true,
                    ..visit
                });
                let at = visit.path.extended(&node.prefix)?;
                if node.value.is_some() {
                    pending.push(ContentId::from_bytes(at.full()?));
                }
                for (suffix, ()) in &node.entries {
                    pending.push(ContentId::from_bytes(at.extended(suffix)?.full()?));
                }
                for &(edge, child) in &node.children {
                    let path = at.extended(&[edge])?;
                    // Checked before the child is read, so a shared subtree costs
                    // one lookup here and no read.
                    if self.visited(child.as_bytes(), path.bytes())? {
                        counts.nodes_skipped += 1;
                    } else {
                        stack.push(Visit {
                            id: child,
                            path,
                            leaving: false,
                        });
                    }
                }
                if pending.len() >= INSERT_BATCH {
                    counts.marked += self.insert(&pending)?;
                    pending.clear();
                }
            }
            counts.marked += self.insert(&pending)?;
            pending.clear();
        }
        Ok(counts)
    }

    /// Records that every head was marked, so the file may be merged and swept
    /// against, and closes it.
    pub fn finish(mut self) -> Result<PathBuf, GcError> {
        self.seal()?;
        Ok(self.path)
    }

    /// Records the file finished, once every mark is on disk: written unsynced, the
    /// marks are synced first, so a crash can leave a finished row only over them.
    fn seal(&mut self) -> Result<(), GcError> {
        self.connection.cache_flush()?;
        std::fs::File::open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(|e| GcError::File(format!("syncing {}: {e}", self.path.display())))?;
        self.connection.pragma_update(None, "synchronous", "FULL")?;
        self.connection.execute(
            "INSERT INTO finished (one, marked) SELECT 1, count(*) FROM marked",
            [],
        )?;
        Ok(())
    }

    /// How many IDs are marked.
    pub fn len(&self) -> Result<u64, GcError> {
        let count: i64 = self
            .connection
            .query_row("SELECT count(*) FROM marked", [], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    }

    /// Whether nothing is marked.
    pub fn is_empty(&self) -> Result<bool, GcError> {
        Ok(self.len()? == 0)
    }

    /// Whether `id` is marked.
    pub fn contains(&self, id: ContentId) -> Result<bool, GcError> {
        Ok(self
            .connection
            .prepare_cached("SELECT 1 FROM marked WHERE id = ?1")?
            .exists(params![id.as_bytes().as_slice()])?)
    }

    fn visited(&self, node: &[u8; 32], prefix: &[u8]) -> Result<bool, GcError> {
        Ok(self
            .connection
            .prepare_cached("SELECT 1 FROM visited WHERE node = ?1")?
            .exists(params![node_key(node, prefix)])?)
    }

    fn record_visited(&self, node: &[u8; 32], prefix: &[u8]) -> Result<(), GcError> {
        self.connection
            .prepare_cached("INSERT OR IGNORE INTO visited (node) VALUES (?1)")?
            .execute(params![node_key(node, prefix)])?;
        Ok(())
    }

    /// Marks `ids`, answering how many were not marked before.
    fn insert(&mut self, ids: &[ContentId]) -> Result<u64, GcError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let transaction = self.connection.transaction()?;
        let mut added = 0;
        {
            let mut insert =
                transaction.prepare_cached("INSERT OR IGNORE INTO marked (id) VALUES (?1)")?;
            for id in ids {
                added += insert.execute(params![id.as_bytes().as_slice()])? as u64;
            }
        }
        transaction.commit()?;
        Ok(added)
    }

    /// Up to `limit` marked IDs whose references are not yet followed.
    fn unfollowed(&self, limit: usize) -> Result<Vec<ContentId>, GcError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT id FROM marked WHERE expanded = 0 LIMIT ?1")?;
        let rows = statement.query_map(params![limit as i64], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| {
            let bytes = row?;
            let digest: [u8; 32] = bytes
                .try_into()
                .map_err(|_| GcError::File("a marked ID is not 32 bytes".into()))?;
            Ok(ContentId::from_digest(digest))
        })
        .collect()
    }

    fn set_followed(&mut self, ids: &[ContentId]) -> Result<(), GcError> {
        let transaction = self.connection.transaction()?;
        {
            let mut update =
                transaction.prepare_cached("UPDATE marked SET expanded = 1 WHERE id = ?1")?;
            for id in ids {
                update.execute(params![id.as_bytes().as_slice()])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// A content set node to read, or, `leaving`, one whose subtree is marked.
#[derive(Clone, Copy)]
struct Visit {
    id: wheat_core::ContentSetId,
    path: Path32,
    leaving: bool,
}

/// The key bytes above a node: at most a content ID's 32.
#[derive(Clone, Copy)]
struct Path32 {
    bytes: [u8; 32],
    len: usize,
}

impl Path32 {
    const EMPTY: Self = Self {
        bytes: [0; 32],
        len: 0,
    };

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn len(&self) -> usize {
        self.len
    }

    /// The path with `more` after it; a node whose keys would run past 32 bytes
    /// does not decode as a content set.
    fn extended(&self, more: &[u8]) -> Result<Self, GcError> {
        let len = self.len + more.len();
        if len > 32 {
            return Err(GcError::ContentSet(CollectionError::Decode(
                wheat_core::CanonicalObjectError::InvalidCollection("content set key too long"),
            )));
        }
        let mut path = *self;
        path.bytes[self.len..len].copy_from_slice(more);
        path.len = len;
        Ok(path)
    }

    /// The whole key, which a value or entry must end on.
    fn full(&self) -> Result<&[u8], GcError> {
        if self.len != 32 {
            return Err(GcError::ContentSet(CollectionError::Decode(
                wheat_core::CanonicalObjectError::InvalidCollection("content set key too short"),
            )));
        }
        Ok(&self.bytes)
    }
}

/// A content set node, keyed by where it sits: the same node bytes at another key
/// position hold other keys.
fn node_key(node: &[u8; 32], prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(32 + prefix.len());
    key.extend_from_slice(node);
    key.extend_from_slice(prefix);
    key
}

/// Marks what the marked records need to be read, to a fixed point (delta bases,
/// chunks, and a long blob's parts and their chunks, read from `blobs`), then
/// reclaims every ID `blobs` lists that is not marked. `marks` must be finished.
#[tracing::instrument(name = "content_gc_sweep", level = "debug", skip_all)]
pub async fn sweep(
    blobs: &Arc<dyn ContentBlobStore>,
    marks: &mut ContentMarks,
    inventory: &dyn ContentInventory,
) -> Result<SweepCounts, GcError> {
    if !marks.finished {
        return Err(GcError::File(format!(
            "{} was not finished, so it may lack marks",
            marks.path.display()
        )));
    }
    let mut counts = SweepCounts::default();
    let content = ContentStore::over_blobs(Arc::clone(blobs));
    loop {
        let batch = marks.unfollowed(REFERENCE_BATCH)?;
        if batch.is_empty() {
            break;
        }
        counts.reference_calls += 1;
        let mut answered = inventory.references(&batch).await?;
        answered.sort_unstable_by_key(|(id, _)| *id);
        let mut referenced = Vec::new();
        for id in &batch {
            let Ok(at) = answered.binary_search_by_key(id, |(id, _)| *id) else {
                return Err(GcError::Unknown(*id));
            };
            match &answered[at].1 {
                RecordReferences::None => {}
                RecordReferences::Delta { base } => referenced.push(*base),
                RecordReferences::Chunks(chunks) => referenced.extend_from_slice(chunks),
                RecordReferences::Parts(parts) => {
                    for part in parts {
                        referenced.push(*part);
                        // The blob store's content store blocks in place itself.
                        let bytes = content.get_bounded(*part, MAX_PART_BYTES)?;
                        let record = decode_content_chunk_part(&bytes)
                            .map_err(|e| GcError::Part(*part, e))?;
                        counts.parts_read += 1;
                        referenced.extend(record.chunks.iter().map(|chunk| chunk.id));
                    }
                }
            }
        }
        counts.referenced += marks.insert(&referenced)?;
        marks.set_followed(&batch)?;
        counts.followed += batch.len() as u64;
    }

    let mut cursor = None;
    let mut unmarked = Vec::with_capacity(SWEEP_PAGE_IDS);
    loop {
        let page = blobs
            .list_page(cursor, SWEEP_PAGE_IDS)
            .await?
            .ok_or(GcError::NotListed)?;
        counts.listed += page.ids.len() as u64;
        counts.pages += 1;
        unmarked.clear();
        for id in page.ids {
            if !marks.contains(id)? {
                unmarked.push(id);
            }
        }
        // A store may answer a longer page than asked: each call stays within the
        // bound.
        for batch in unmarked.chunks(SWEEP_PAGE_IDS) {
            counts.reclaim_calls += 1;
            let reclaimed = inventory.reclaim(batch).await?.len() as u64;
            counts.reclaimed += reclaimed;
            counts.kept += batch.len() as u64 - reclaimed.min(batch.len() as u64);
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(counts)
}

/// `time` in milliseconds since the Unix epoch, as a mark file records it.
fn unix_ms(time: SystemTime) -> Result<i64, GcError> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .ok_or_else(|| GcError::File("a collection's start must be after 1970".into()))
}

fn from_unix_ms(at_ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(at_ms.unsigned_abs())
}
