//! Durable publication catalog for immutable and active packed segments.
//!
//! `catalog.head` and the newest verified frame of the active segment's log are
//! authority. SQLite is disposable derived state and is rebuilt from the
//! authenticated immutable chain plus the active segment whenever its combined
//! checkpoint disagrees.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::debug_span;
use wheat_core::{
    PhysicalCatalogRef, PhysicalCatalogSnapshot, PhysicalContentGeneration,
    PhysicalSegmentDescriptor, RepackIntent, SegmentId, SegmentIndexEntry, SegmentRecordError,
    decode_physical_catalog_head, decode_physical_catalog_snapshot, decode_repack_intent,
    encode_physical_catalog_head, encode_physical_catalog_snapshot, encode_repack_intent,
};

use super::{
    active::{ActiveError, ActivePublishedState, ActiveSegment, ActiveWrite, ActiveWritten},
    bulk::{BulkSegment, FinalizedBulk},
    segment::{
        CheckedRecord, MAX_SEGMENT_PAYLOAD_BYTES, METADATA_NAMESPACE, PreparedSegmentRecord,
        SealedSegment, SegmentBuilder, SegmentError, SegmentReader, SegmentRecord,
        prepare_delta_record, prepare_record, verify_logical,
    },
    trace::PackedStoreTraceCounters,
};
use crate::Durability;

const SCHEMA: &str = include_str!("catalog_sql/schema.sql");
const LOOKUP: &str = include_str!("catalog_sql/lookup.sql");
const SCHEMA_VERSION: i64 = 6;
const SEGMENT_SUFFIX: &str = ".segment";
const CATALOG_SUFFIX: &str = ".catalog";
const CATALOG_HEAD_FILE: &str = "catalog.head";
const ACTIVE_FILE: &str = "active.segment";
const BULK_FILE_PREFIX: &str = "bulk-";
const REPACK_INTENT_FILE: &str = "repack.intent";
const DEFAULT_ACTIVE_TARGET_BYTES: u64 = super::active::ACTIVE_SEGMENT_TARGET_BYTES;

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("packed catalog is already open by another process")]
    Busy,
    #[error("catalog I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("catalog database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("catalog record failed: {0}")]
    Record(#[from] SegmentRecordError),
    #[error("segment failed: {0}")]
    Segment(#[from] SegmentError),
    #[error("active segment failed: {0}")]
    Active(#[from] ActiveError),
    #[error("catalog chain is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("catalog must be reopened after an interrupted seal")]
    Poisoned,
    /// A read beside an append found the locator in need of repair, which waits
    /// for the append. [`SharedCatalog`] retries the read once it is done.
    #[error("the locator needs repair, which waits for the append in progress")]
    AppendInFlight,
    #[error("packed catalog lock is poisoned: a holder panicked")]
    LockPoisoned,
    #[error("repack requires durable storage without pending deferred writes")]
    RepackUnsupported,
    #[error("repack inventory exceeds its {limit} record limit")]
    RepackInventoryLimit { limit: usize },
    #[error("repack plan no longer matches physical authority")]
    RepackStaleAuthority,
}

#[derive(Clone)]
struct SealedSource {
    path: String,
    generation: u64,
    sequence: u64,
    segment: [u8; 32],
    stored_length: u64,
}

/// Where repack reads a record body from. `Segment` sorts first, so when a
/// loose object also has a segment copy, deduplication keeps the segment one.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RecordSource {
    Segment([u8; 32]),
    Loose,
}

#[derive(Clone, Copy)]
struct RepackInventoryEntry {
    source: RecordSource,
    namespace: u8,
    kind: u16,
    digest: [u8; 32],
    logical_length: u64,
}

/// A validated loose metadata object offered to repack for absorption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LooseMetadataRecord {
    pub digest: [u8; 32],
    pub kind: u16,
    pub logical_length: u64,
}

/// Metadata inputs to a structural repack.
pub(super) struct MetadataRepackInput<'a> {
    /// Planned physical delta bases: older target -> newer base.
    pub bases: &'a BTreeMap<[u8; 32], [u8; 32]>,
    /// Loose metadata objects to move into segments.
    pub loose: &'a [LooseMetadataRecord],
    /// Reads one loose object's verified canonical bytes.
    pub read_loose: &'a mut dyn FnMut([u8; 32]) -> Result<Vec<u8>, CatalogError>,
}

struct DeferredBulk {
    source: String,
    segment: BulkSegment,
}

type CatalogHead = Option<(u64, [u8; 32])>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CatalogPhysicalAuthority {
    pub head: CatalogHead,
    pub content: PhysicalContentGeneration,
    pub active: ActivePublishedState,
}
type CatalogCheckpoint = (CatalogHead, ActivePublishedState, u64);
type LocatorRow = (u16, u64, u8, u64, u64, String);

/// What a keyed catalog read fetches.
#[derive(Clone, Copy, Debug)]
enum ReadMode {
    /// The verified logical payload, refused above `max` bytes before it is read.
    Payload { max: usize },
    /// Only the logical length from the authenticated index entry; no payload is read.
    Length,
}

/// A keyed catalog read's result, shaped by its [`ReadMode`].
enum Found {
    Payload { kind: u16, bytes: Vec<u8> },
    Length(u64),
}

/// A key's authenticated index entry, carrying its payload only in [`ReadMode::Payload`].
struct Authority {
    entry: SegmentIndexEntry,
    payload: Option<Vec<u8>>,
}

impl Found {
    fn into_payload(self) -> (u16, Vec<u8>) {
        match self {
            Self::Payload { kind, bytes } => (kind, bytes),
            Self::Length(_) => unreachable!("a payload read returns a payload"),
        }
    }

    fn into_length(self) -> u64 {
        match self {
            Self::Length(length) => length,
            Self::Payload { .. } => unreachable!("a length read returns a length"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogRepackReport {
    pub no_op: bool,
    pub generation_before: u64,
    pub generation_after: u64,
    pub segments_before: usize,
    pub segments_after: usize,
    pub physical_bytes_before: u64,
    pub physical_bytes_after: u64,
    pub structural_records: u64,
    /// Number of source-body reads performed by the streaming rewrite.
    pub source_body_reads: u64,
    /// Peak simultaneously resident record bodies: the record being written,
    /// the previous one (usually its delta base), and at most one re-read base.
    pub max_resident_bodies: u64,
    pub bytes_written: u64,
    pub bytes_retired: u64,
    /// Metadata records stored as in-segment deltas, and their deepest chain.
    pub delta_records: u64,
    pub max_delta_depth: u8,
    /// Loose metadata objects whose bytes are now in the selected segments.
    /// Their loose files are redundant once the catalog head is durable.
    pub absorbed_loose: Vec<[u8; 32]>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CatalogRepackOptions {
    pub max_records: usize,
    pub target_segment_bytes: u64,
}

/// Opaque authority issued only after store-level pack authentication. Keeping
/// this distinct from a physical generation token prevents catalog-only callers
/// from authorizing content retirement.
pub(super) struct ValidatedContentGeneration<'a> {
    authority: PhysicalContentGeneration,
    manifest: &'a crate::ContentGenerationManifest,
}

impl<'a> ValidatedContentGeneration<'a> {
    pub(super) fn after_pack_verification(
        authority: PhysicalContentGeneration,
        manifest: &'a crate::ContentGenerationManifest,
    ) -> Self {
        Self {
            authority,
            manifest,
        }
    }

    fn manifest_for(
        &self,
        authority: PhysicalContentGeneration,
    ) -> Option<&'a crate::ContentGenerationManifest> {
        (self.authority == authority).then_some(self.manifest)
    }
}

/// A process-exclusive writer/read facade over a packed-store root.
pub(crate) struct PackedCatalog {
    root: PathBuf,
    segments: PathBuf,
    catalogs: PathBuf,
    _lock: File,
    index: Connection,
    /// A second connection to the locator, for appends: an [`AppendJob`] takes it
    /// and writes its rows while `index` serves reads.
    append_index: Option<Connection>,
    /// Between [`Self::begin_append`] and [`Self::finish_append`].
    append_in_flight: bool,
    generation: Option<u64>,
    head: Option<[u8; 32]>,
    content_generation: PhysicalContentGeneration,
    sealed_sources: Vec<SealedSource>,
    sealed_readers: HashMap<[u8; 32], SegmentReader>,
    active: ActiveSegment,
    bulk: Option<DeferredBulk>,
    durability: Durability,
    active_target_bytes: u64,
    max_segment_entries: usize,
    max_segment_payload_bytes: u64,
    locator_rows: u64,
    poisoned: bool,
    trace: Option<Arc<PackedStoreTraceCounters>>,
    #[cfg(test)]
    sealed_authentications: usize,
    #[cfg(test)]
    seal_failpoint: Option<SealFailpoint>,
    #[cfg(test)]
    append_failpoint: Option<AppendFailpoint>,
    #[cfg(test)]
    append_pause: Option<AppendPause>,
    #[cfg(test)]
    deferred_failpoint: Option<DeferredFailpoint>,
    #[cfg(test)]
    close_failpoint: Option<CloseFailpoint>,
    #[cfg(test)]
    repack_failpoint: Option<RepackFailpoint>,
}

/// A [`PackedCatalog`] shared between threads. Writers take turns on `writer`; a
/// durable append does its I/O holding only that, so reads of published records
/// go on meanwhile instead of waiting out the append's fsyncs. Every operation
/// other than a read or a put is exclusive: it waits for any append to finish
/// and holds off the next.
pub(crate) struct SharedCatalog {
    writer: Mutex<()>,
    catalog: Mutex<PackedCatalog>,
}

/// Exclusive use of a [`SharedCatalog`]: no append is in progress.
pub(crate) struct ExclusiveCatalog<'a> {
    _writer: MutexGuard<'a, ()>,
    catalog: MutexGuard<'a, PackedCatalog>,
}

impl Deref for ExclusiveCatalog<'_> {
    type Target = PackedCatalog;

    fn deref(&self) -> &PackedCatalog {
        &self.catalog
    }
}

impl DerefMut for ExclusiveCatalog<'_> {
    fn deref_mut(&mut self) -> &mut PackedCatalog {
        &mut self.catalog
    }
}

impl SharedCatalog {
    pub(crate) fn new(catalog: PackedCatalog) -> Self {
        Self {
            writer: Mutex::new(()),
            catalog: Mutex::new(catalog),
        }
    }

    pub(crate) fn into_inner(self) -> Result<PackedCatalog, CatalogError> {
        self.catalog
            .into_inner()
            .map_err(|_| CatalogError::LockPoisoned)
    }

    /// Waits for any append to finish and holds off the next.
    pub(crate) fn exclusive(&self) -> Result<ExclusiveCatalog<'_>, CatalogError> {
        let writer = debug_span!("packed_writer_wait").in_scope(|| self.writer.lock());
        Ok(ExclusiveCatalog {
            _writer: writer.map_err(|_| CatalogError::LockPoisoned)?,
            catalog: self.lock()?,
        })
    }

    /// The catalog between appends' steps. Only for reads: an append may be in
    /// progress.
    fn lock(&self) -> Result<MutexGuard<'_, PackedCatalog>, CatalogError> {
        debug_span!("packed_catalog_wait")
            .in_scope(|| self.catalog.lock())
            .map_err(|_| CatalogError::LockPoisoned)
    }

    /// See [`PackedCatalog::put_records`].
    pub(crate) fn put_records(&self, records: &[CheckedRecord<'_>]) -> Result<(), CatalogError> {
        let mut exclusive = self.exclusive()?;
        if exclusive.durability == Durability::Deferred {
            return exclusive.put_records(records);
        }
        let ExclusiveCatalog {
            _writer: _turn,
            mut catalog,
        } = exclusive;
        let mut put = catalog.begin_durable_put(records)?;
        while let Some(job) = catalog.next_append(&mut put)? {
            drop(catalog);
            let outcome = job.run();
            catalog = self.lock()?;
            catalog.finish_put_append(&put, outcome)?;
        }
        Ok(())
    }

    /// Runs `read` beside any append in progress, or after it when the read needs
    /// the locator repaired.
    fn read<T>(
        &self,
        read: impl Fn(&mut PackedCatalog) -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        let result = read(&mut *self.lock()?);
        match result {
            Err(CatalogError::AppendInFlight) => read(&mut *self.exclusive()?),
            result => result,
        }
    }

    /// See [`PackedCatalog::get`].
    pub(crate) fn get(
        &self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        self.read(|catalog| catalog.get(namespace, digest))
    }

    /// See [`PackedCatalog::get_bounded`].
    pub(crate) fn get_bounded(
        &self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        self.read(|catalog| catalog.get_bounded(namespace, digest, max))
    }

    /// See [`PackedCatalog::logical_length`].
    pub(crate) fn logical_length(
        &self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<u64>, CatalogError> {
        self.read(|catalog| catalog.logical_length(namespace, digest))
    }

    /// The catalog for a read of its published state, beside any append in
    /// progress.
    pub(crate) fn published(&self) -> Result<MutexGuard<'_, PackedCatalog>, CatalogError> {
        self.lock()
    }
}

/// The prepared records of a durable put that are still to be appended.
struct DurablePut<'a> {
    prepared: Vec<PreparedSegmentRecord<'a>>,
}

/// The durable I/O of one append, done without the catalog: the frame written
/// to the active segment's log and synced, then the locator rows, on a
/// connection of the job's own.
struct AppendJob<'a> {
    durability: Durability,
    write: ActiveWrite<'a>,
    index: Connection,
    catalog_head: CatalogHead,
    locator_rows: u64,
    #[cfg(test)]
    failpoint: Option<AppendFailpoint>,
    #[cfg(test)]
    pause: Option<AppendPause>,
}

/// How far an [`AppendJob`] got: the segment write's outcome, then the locator
/// row count after indexing what it added, or the indexing error.
struct AppendOutcome {
    index: Connection,
    written: ActiveWritten,
    indexed: Result<u64, CatalogError>,
}

impl AppendJob<'_> {
    #[tracing::instrument(name = "packed_append", level = "debug", skip_all)]
    fn run(self) -> AppendOutcome {
        let Self {
            durability,
            write,
            mut index,
            catalog_head,
            locator_rows,
            #[cfg(test)]
            failpoint,
            #[cfg(test)]
            pause,
        } = self;
        #[cfg(test)]
        let pause_at = |point| {
            if let Some(pause) = pause.as_ref().filter(|pause| pause.at == point) {
                pause.wait();
            }
        };
        let written = debug_span!("packed_append_segment").in_scope(|| {
            let mut written = write.run();
            written.sync(durability);
            #[cfg(test)]
            if failpoint == Some(AppendFailpoint::Sync) {
                written.test_fail();
            }
            written
        });
        #[cfg(test)]
        pause_at(AppendPausePoint::SegmentWritten);
        let indexed = match written.appended() {
            None => Ok(locator_rows),
            #[cfg(test)]
            Some(_) if failpoint == Some(AppendFailpoint::Synced) => Err(CatalogError::Corrupt(
                "test failure after the synced append",
            )),
            Some((state, added)) => debug_span!("packed_append_index").in_scope(|| {
                index_appended_entries(&mut index, added, catalog_head, state, locator_rows)
            }),
        };
        #[cfg(test)]
        pause_at(AppendPausePoint::Indexed);
        AppendOutcome {
            index,
            written,
            indexed,
        }
    }
}

/// Where a test stops an [`AppendJob`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppendPausePoint {
    /// The records are in the segment's log, which the catalog has not taken on.
    SegmentWritten,
    /// The locator holds the records' rows; the catalog has not taken them on.
    Indexed,
}

#[cfg(test)]
struct AppendPause {
    at: AppendPausePoint,
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl AppendPause {
    fn wait(&self) {
        // A test that has dropped its handle is not waiting to resume the append.
        if self.reached.send(()).is_ok() {
            let _ = self.resume.recv();
        }
    }
}

/// The test's side of an [`AppendPause`].
#[cfg(test)]
pub(crate) struct AppendPauseHandle {
    pub(crate) reached: std::sync::mpsc::Receiver<()>,
    pub(crate) resume: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppendFailpoint {
    /// Syncing the written frame fails.
    Sync,
    /// Indexing fails after the frame is synced.
    Synced,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredFailpoint {
    IndexCommit,
    AfterCatalogHead,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealFailpoint {
    SqliteAfterHead,
    DuringRetirement,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseFailpoint {
    BeforeFinalize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepackFailpoint {
    AfterIntent,
    AfterHead,
}

impl PackedCatalog {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        durability: Durability,
    ) -> Result<Self, CatalogError> {
        Self::open_with_target(root, durability, DEFAULT_ACTIVE_TARGET_BYTES, None)
    }

    pub(crate) fn open_with_trace(
        root: impl AsRef<Path>,
        durability: Durability,
        trace: Arc<PackedStoreTraceCounters>,
    ) -> Result<Self, CatalogError> {
        Self::open_with_target(root, durability, DEFAULT_ACTIVE_TARGET_BYTES, Some(trace))
    }

    fn open_with_target(
        root: impl AsRef<Path>,
        durability: Durability,
        active_target_bytes: u64,
        trace: Option<Arc<PackedStoreTraceCounters>>,
    ) -> Result<Self, CatalogError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let segments = root.join("segments");
        let catalogs = root.join("catalog");
        fs::create_dir_all(&segments)?;
        fs::create_dir_all(&catalogs)?;
        sync_created_ancestors(&root, durability)?;

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("catalog.lock"))?;
        try_lock_exclusive(&lock)?;

        // Recovery is deliberately completed under the writer lock before the
        // disposable locator is opened or trusted.
        let catalog_head_path = root.join(CATALOG_HEAD_FILE);
        let durable_head = match read_catalog_head(&catalog_head_path) {
            Ok(head) => head,
            Err(CatalogError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                let head = latest_catalog_head(&catalogs, &segments)?;
                write_catalog_head(&root, &catalog_head_path, head, durability)?;
                head
            }
            Err(error) => return Err(error),
        };
        recover_repack_intent(&root, durable_head, durability)?;
        let authority = read_catalog_authority(&catalogs, durable_head)?;
        let active = recover_active(&root, durability)?;
        remove_unpublished_bulk_files(&segments, durability)?;
        remove_unreachable_publications(
            &segments,
            &catalogs,
            durable_head,
            authority.as_ref(),
            durability,
        )?;

        let index_path = root.join("catalog-locator.sqlite3");
        let index_was_missing = !index_path.exists();
        let mut index_recreated = false;
        let index = open_index(&index_path, durability).or_else(|_| {
            remove_index_files(&index_path)?;
            index_recreated = true;
            open_index(&index_path, durability)
        })?;
        if index_was_missing || index_recreated {
            durability.sync_directory(&root)?;
        }
        let append_index = connect_index(&index_path, durability)?;
        let sealed_sources = authority.as_ref().map_or_else(Vec::new, authority_sources);
        let mut catalog = Self {
            root,
            segments,
            catalogs,
            _lock: lock,
            index,
            append_index: Some(append_index),
            append_in_flight: false,
            generation: durable_head.map(|(generation, _)| generation),
            head: durable_head.map(|(_, digest)| digest),
            content_generation: authority
                .as_ref()
                .map_or_else(no_content_generation, |snapshot| snapshot.content),
            sealed_sources,
            sealed_readers: HashMap::new(),
            active,
            bulk: None,
            durability,
            active_target_bytes,
            max_segment_entries: super::segment::MAX_SEGMENT_ENTRIES,
            max_segment_payload_bytes: MAX_SEGMENT_PAYLOAD_BYTES,
            locator_rows: 0,
            poisoned: false,
            trace,
            #[cfg(test)]
            sealed_authentications: 0,
            #[cfg(test)]
            seal_failpoint: None,
            #[cfg(test)]
            append_failpoint: None,
            #[cfg(test)]
            append_pause: None,
            #[cfg(test)]
            deferred_failpoint: None,
            #[cfg(test)]
            close_failpoint: None,
            #[cfg(test)]
            repack_failpoint: None,
        };
        let authority = (durable_head, catalog.active.state());
        let checkpoint = catalog.read_checkpoint();
        let checkpoint_valid = checkpoint
            .as_ref()
            .is_ok_and(|(head, active, _)| (*head, *active) == authority);
        if !checkpoint_valid {
            catalog.rebuild_index()?;
        } else {
            catalog.locator_rows = checkpoint.expect("validated checkpoint").2;
            if catalog.recount_locator_rows()? != catalog.locator_rows {
                catalog.rebuild_index()?;
            }
        }
        Ok(catalog)
    }

    #[cfg(test)]
    pub(crate) fn open_with_test_threshold(
        root: impl AsRef<Path>,
        durability: Durability,
        active_target_bytes: u64,
    ) -> Result<Self, CatalogError> {
        Self::open_with_target(root, durability, active_target_bytes, None)
    }

    #[cfg(test)]
    pub(crate) fn open_with_test_limits(
        root: impl AsRef<Path>,
        durability: Durability,
        active_target_bytes: u64,
        max_segment_entries: usize,
        max_segment_payload_bytes: u64,
    ) -> Result<Self, CatalogError> {
        let mut catalog = Self::open_with_target(root, durability, active_target_bytes, None)?;
        assert!(max_segment_entries <= super::segment::MAX_SEGMENT_ENTRIES);
        assert!(max_segment_payload_bytes <= MAX_SEGMENT_PAYLOAD_BYTES);
        catalog.max_segment_entries = max_segment_entries;
        catalog.max_segment_payload_bytes = max_segment_payload_bytes;
        Ok(catalog)
    }

    #[cfg(test)]
    pub(crate) fn test_set_seal_failpoint(&mut self, failpoint: SealFailpoint) {
        self.seal_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn test_set_append_failpoint(&mut self, failpoint: AppendFailpoint) {
        self.append_failpoint = Some(failpoint);
    }

    /// Makes the next append stop at `at` until the test lets it go on.
    #[cfg(test)]
    pub(crate) fn test_pause_next_append(&mut self, at: AppendPausePoint) -> AppendPauseHandle {
        let (reached_sender, reached) = std::sync::mpsc::channel();
        let (resume, resume_receiver) = std::sync::mpsc::channel();
        self.append_pause = Some(AppendPause {
            at,
            reached: reached_sender,
            resume: resume_receiver,
        });
        AppendPauseHandle { reached, resume }
    }

    #[cfg(test)]
    pub(crate) fn test_set_deferred_failpoint(&mut self, failpoint: DeferredFailpoint) {
        self.deferred_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn test_set_close_failpoint(&mut self, failpoint: CloseFailpoint) {
        self.close_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn test_set_repack_failpoint(&mut self, failpoint: RepackFailpoint) {
        self.repack_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn test_locator_rows(&self) -> u64 {
        self.locator_rows
    }

    #[cfg(test)]
    pub(crate) fn test_checkpoint_locator_rows(&self) -> Result<u64, CatalogError> {
        Ok(self.read_checkpoint()?.2)
    }

    #[cfg(test)]
    pub(crate) fn test_counts(&self) -> Result<(u64, u64, u64), CatalogError> {
        let locator = self
            .index
            .query_row("SELECT count(*) FROM locator", [], |row| row.get(0))?;
        let sources = self
            .index
            .query_row("SELECT count(*) FROM physical_source", [], |row| row.get(0))?;
        let catalogs = fs::read_dir(&self.catalogs)?.count() as u64;
        Ok((locator, sources, catalogs))
    }

    #[cfg(test)]
    pub(crate) fn test_sealed_authentications(&self) -> usize {
        self.sealed_authentications
    }

    #[cfg(test)]
    pub(crate) fn test_reject_locator_rewrite(&self, digest: [u8; 32]) -> Result<(), CatalogError> {
        let digest = hex(&digest);
        self.index.execute_batch(&format!(
            "CREATE TRIGGER preserve_existing_locator_update BEFORE UPDATE ON locator \
             WHEN OLD.digest = x'{digest}' \
             BEGIN SELECT RAISE(ABORT, 'existing locator updated'); END; \
             CREATE TRIGGER preserve_existing_locator_delete BEFORE DELETE ON locator \
             WHEN OLD.digest = x'{digest}' \
             BEGIN SELECT RAISE(ABORT, 'existing locator deleted'); END;"
        ))?;
        Ok(())
    }

    /// Appends records according to the selected durability policy.
    ///
    /// Durable stores republish a crash-safe authenticated active tail. Deferred
    /// stores append payloads once to an unpublished bulk file and only emit its
    /// tail when it rolls or this catalog is consumed by `close`.
    pub(crate) fn put_records(
        &mut self,
        records: &[CheckedRecord<'_>],
    ) -> Result<(), CatalogError> {
        if self.durability == Durability::Deferred {
            return self.put_records_deferred(records);
        }
        self.put_records_durable(records)
    }

    fn put_records_durable(&mut self, records: &[CheckedRecord<'_>]) -> Result<(), CatalogError> {
        let mut put = self.begin_durable_put(records)?;
        while let Some(job) = self.next_append(&mut put)? {
            let outcome = job.run();
            self.finish_put_append(&put, outcome)?;
        }
        Ok(())
    }

    /// Prepares the `records` this catalog lacks, for [`Self::next_append`].
    fn begin_durable_put<'a>(
        &mut self,
        records: &[CheckedRecord<'a>],
    ) -> Result<DurablePut<'a>, CatalogError> {
        if self.poisoned {
            return Err(CatalogError::Poisoned);
        }
        if records.is_empty() {
            return Ok(DurablePut {
                prepared: Vec::new(),
            });
        }
        // Resolve and deduplicate keys before projecting the active size. A
        // duplicate-heavy batch must not cause a false pre-seal.
        let mut pending: Vec<CheckedRecord<'a>> = Vec::with_capacity(records.len());
        let mut pending_by_key: HashMap<(u8, [u8; 32]), usize> =
            HashMap::with_capacity(records.len());
        for checked in records {
            let record = checked.record();
            if let Some(&index) = pending_by_key.get(&(record.namespace, record.digest)) {
                let previous: SegmentRecord<'_> = pending[index].record();
                if previous.kind != record.kind || previous.bytes != record.bytes {
                    return Err(CatalogError::Corrupt(
                        "duplicate batch key has conflicting bytes",
                    ));
                }
                continue;
            }
            if let Some((kind, bytes)) = self.get(record.namespace, record.digest)? {
                if kind != record.kind || bytes != record.bytes {
                    return Err(CatalogError::Corrupt(
                        "existing key differs from proposed bytes",
                    ));
                }
            } else {
                pending_by_key.insert((record.namespace, record.digest), pending.len());
                pending.push(*checked);
            }
        }
        // Prepare every new logical record exactly once. Partitioning and all
        // downstream writers operate on these stored bytes, while duplicate and
        // existing-key checks above deliberately compare the logical bytes.
        let prepared = pending
            .into_iter()
            .map(prepare_record)
            .collect::<Result<Vec<_>, _>>()?;
        // Reject any individually impossible record before sealing or appending
        // earlier records from this call. The target is advisory for one record,
        // but the format's count and stored-payload bounds are not.
        for record in &prepared {
            self.projected_physical_size(0, 0, 1, record.stored_length())?;
        }
        Ok(DurablePut { prepared })
    }

    /// The append of the next batch of `put`, sealing the active segment first
    /// when the batch does not fit it, or `None` when `put` is done.
    fn next_append<'a>(
        &mut self,
        put: &mut DurablePut<'a>,
    ) -> Result<Option<AppendJob<'a>>, CatalogError> {
        let prepared = &mut put.prepared;
        if prepared.is_empty() {
            return Ok(None);
        }
        // Partition at the catalog boundary. Adapters may submit arbitrarily
        // large batches; each active append remains bounded by the format and
        // target, while a record which cannot fit an empty segment still fails.
        let mut end = 0;
        let mut payload = 0_u64;
        while end < prepared.len() {
            let Some(next_payload) = payload.checked_add(prepared[end].stored_length()) else {
                break;
            };
            let projected = self.projected_physical_size(
                self.active.entries().len(),
                self.active.state().payload_bytes,
                end + 1,
                next_payload,
            );
            let fits = projected
                .as_ref()
                .is_ok_and(|&size| size <= self.active_target_bytes);
            if !fits && end != 0 {
                break;
            }
            if !fits && self.active.state().object_count != 0 {
                self.seal_active()?;
                payload = 0;
                end = 0;
                continue;
            }
            payload = next_payload;
            end += 1;
            if projected.is_ok_and(|size| size > self.active_target_bytes) {
                break;
            }
        }
        if end == 0 {
            return Err(SegmentError::TooLarge.into());
        }
        self.begin_append(prepared.drain(..end).collect()).map(Some)
    }

    /// Finishes one append of `put`, then seals the active segment when the rest of
    /// `put` will not fit or the segment is full.
    fn finish_put_append(
        &mut self,
        put: &DurablePut<'_>,
        outcome: AppendOutcome,
    ) -> Result<(), CatalogError> {
        self.finish_append(outcome)?;
        if !put.prepared.is_empty()
            || self.active.state().total_length >= self.active_target_bytes
            || self.active.should_seal()
        {
            self.seal_active()?;
        }
        Ok(())
    }

    fn projected_physical_size(
        &self,
        existing_count: usize,
        existing_payload: u64,
        additional_count: usize,
        additional_payload: u64,
    ) -> Result<u64, CatalogError> {
        let count = existing_count
            .checked_add(additional_count)
            .ok_or(SegmentError::TooManyEntries)?;
        if count > self.max_segment_entries {
            return Err(SegmentError::TooManyEntries.into());
        }
        let payload = existing_payload
            .checked_add(additional_payload)
            .ok_or(SegmentError::TooLarge)?;
        if payload > self.max_segment_payload_bytes {
            return Err(SegmentError::TooLarge.into());
        }
        // Active segments hold whole records only; deltas are written by repack.
        Ok(super::segment::projected_total_length(count, 0, payload)?)
    }

    /// Starts one durable append of `records` to the active segment. The returned
    /// job does its I/O without this catalog, which keeps serving reads of
    /// published records until [`Self::finish_append`] takes the job's outcome.
    /// Nothing else may change the catalog in between.
    fn begin_append<'a>(
        &mut self,
        records: Vec<PreparedSegmentRecord<'a>>,
    ) -> Result<AppendJob<'a>, CatalogError> {
        assert!(!self.append_in_flight, "one append at a time");
        let intended_generation = self
            .active
            .state()
            .generation
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("active generation overflow"))?;
        let prepared = self
            .active
            .prepare_prepared_append(records, intended_generation)?;
        let index = self
            .append_index
            .take()
            .ok_or(CatalogError::Corrupt("append index connection is in use"))?;
        let write = match self.active.begin_append(prepared) {
            Ok(Ok(write)) => write,
            Ok(Err(_)) => unreachable!("a prepared append of new records writes them"),
            Err(error) => {
                self.append_index = Some(index);
                return Err(error.into());
            }
        };
        // From the frame's write onward an error can leave observable state
        // behind. Keep this instance closed to writes until the segment and the
        // derived index both have the append.
        self.poisoned = true;
        self.append_in_flight = true;
        if let Some(trace) = &self.trace {
            trace.durable_append();
        }
        Ok(AppendJob {
            durability: self.durability,
            write,
            index,
            catalog_head: self.generation.zip(self.head),
            locator_rows: self.locator_rows,
            #[cfg(test)]
            failpoint: self.append_failpoint,
            #[cfg(test)]
            pause: self.append_pause.take(),
        })
    }

    /// Publishes what an [`AppendJob`] appended, or truncates its failed frame.
    fn finish_append(&mut self, outcome: AppendOutcome) -> Result<(), CatalogError> {
        self.append_index = Some(outcome.index);
        self.append_in_flight = false;
        self.active.finish_append(outcome.written)?;
        self.locator_rows = outcome.indexed?;
        self.poisoned = false;
        Ok(())
    }

    fn put_records_deferred(&mut self, records: &[CheckedRecord<'_>]) -> Result<(), CatalogError> {
        if self.poisoned {
            return Err(CatalogError::Poisoned);
        }
        if let Some(trace) = &self.trace {
            trace.deferred_put(records.len());
        }
        for checked in records {
            let record = checked.record();
            if let Some((kind, bytes)) = self.get(record.namespace, record.digest)? {
                if let Some(trace) = &self.trace {
                    trace.deferred_duplicate();
                }
                if kind != record.kind || bytes != record.bytes {
                    return Err(CatalogError::Corrupt(
                        "existing key differs from proposed bytes",
                    ));
                }
                continue;
            }
            let prepared = prepare_record(*checked)?;
            let record_length = prepared.stored_length();
            if self.bulk.is_none() {
                self.create_bulk()?;
            }
            let must_roll = {
                let bulk = self.bulk.as_ref().expect("bulk was created");
                !bulk.segment.is_empty()
                    && match bulk.segment.projected_physical_size(1, record_length) {
                        Ok(length) => length > self.active_target_bytes,
                        Err(_) => true,
                    }
            };
            if must_roll {
                if let Some(trace) = &self.trace {
                    trace.bulk_roll();
                }
                self.finalize_bulk()?;
                self.create_bulk()?;
            }
            let bulk = self.bulk.as_mut().expect("bulk was created");
            // An append can partially write before reporting an I/O error. Do not
            // permit this handle to finalize or publish that file unless its row
            // transaction also commits.
            self.poisoned = true;
            let append_started = Instant::now();
            let entry = bulk.segment.append_prepared(prepared)?;
            if let Some(trace) = &self.trace {
                trace.deferred_append();
                trace.deferred_append_duration(append_started.elapsed());
            }
            let transaction_started = Instant::now();
            let transaction = self.index.transaction()?;
            insert_source_entries(&transaction, &bulk.source, std::slice::from_ref(&entry))?;
            #[cfg(test)]
            if self.deferred_failpoint == Some(DeferredFailpoint::IndexCommit) {
                return Err(CatalogError::Corrupt(
                    "test deferred index transaction failure",
                ));
            }
            transaction.commit()?;
            if let Some(trace) = &self.trace {
                trace.deferred_index_transaction();
                trace.deferred_index_transaction_duration(transaction_started.elapsed());
            }
            self.locator_rows = self
                .locator_rows
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("locator count overflow"))?;
            self.poisoned = false;
        }
        Ok(())
    }

    fn create_bulk(&mut self) -> Result<(), CatalogError> {
        let generation = self
            .generation
            .map_or(Some(0), |value| value.checked_add(1))
            .ok_or(CatalogError::Corrupt("generation overflow"))?;
        let source = format!("segments/{BULK_FILE_PREFIX}{generation}.segment");
        self.bulk = Some(DeferredBulk {
            segment: BulkSegment::create(self.root.join(&source))?,
            source,
        });
        Ok(())
    }

    pub(super) fn finalize_bulk(&mut self) -> Result<(), CatalogError> {
        if self.poisoned {
            return Err(CatalogError::Poisoned);
        }
        let Some(bulk) = self.bulk.take() else {
            return Ok(());
        };
        if bulk.segment.is_empty() {
            return Ok(());
        }
        self.poisoned = true;
        let started = Instant::now();
        let finalized = bulk.segment.finalize()?;
        let result = self.publish_finalized_bulk(finalized, &bulk.source);
        if let Some(trace) = &self.trace {
            trace.bulk_finalization();
            trace.bulk_finalization_duration(started.elapsed());
        }
        result
    }

    fn publish_finalized_bulk(
        &mut self,
        finalized: FinalizedBulk,
        old_source: &str,
    ) -> Result<(), CatalogError> {
        let generation = self
            .generation
            .map_or(Some(0), |value| value.checked_add(1))
            .ok_or(CatalogError::Corrupt("generation overflow"))?;
        let segment_name = format!(
            "{}-0-{}{}",
            generation,
            hex(&finalized.id.0),
            SEGMENT_SUFFIX
        );
        let segment_path = self.segments.join(&segment_name);
        fs::rename(&finalized.path, &segment_path)?;
        let superseded = self.generation.zip(self.head);
        let digest = self.publish_appended_authority(
            generation,
            finalized.id.0,
            finalized.metadata.total_length,
        )?;
        write_catalog_head(
            &self.root,
            &self.root.join(CATALOG_HEAD_FILE),
            Some((generation, digest)),
            self.durability,
        )?;
        #[cfg(test)]
        if self.deferred_failpoint == Some(DeferredFailpoint::AfterCatalogHead) {
            return Err(CatalogError::Corrupt(
                "test deferred failure after catalog head",
            ));
        }
        let new_source = format!("segments/{segment_name}");
        let transaction = self.index.transaction()?;
        transaction.execute(
            "UPDATE physical_source SET path=?1 WHERE path=?2",
            params![new_source, old_source],
        )?;
        write_checkpoint(
            &transaction,
            Some((generation, digest)),
            self.active.state(),
            self.locator_rows,
        )?;
        transaction.commit()?;
        self.generation = Some(generation);
        self.head = Some(digest);
        self.sealed_sources.push(SealedSource {
            path: new_source,
            generation,
            sequence: 0,
            segment: finalized.id.0,
            stored_length: finalized.metadata.total_length,
        });
        remove_superseded_catalog(&self.catalogs, superseded, self.durability)?;
        self.poisoned = false;
        Ok(())
    }

    pub(crate) fn get(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        self.get_bounded(namespace, digest, usize::MAX)
    }

    pub(crate) fn get_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        Ok(self
            .read_traced(namespace, digest, ReadMode::Payload { max })?
            .map(Found::into_payload))
    }

    /// The logical length of a record, from its authenticated index entry. The payload
    /// is not read, so callers that only need sizes do not pay for the bytes.
    pub(crate) fn logical_length(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<u64>, CatalogError> {
        Ok(self
            .read_traced(namespace, digest, ReadMode::Length)?
            .map(Found::into_length))
    }

    fn read_traced(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        mode: ReadMode,
    ) -> Result<Option<Found>, CatalogError> {
        let started = Instant::now();
        if let Some(trace) = &self.trace {
            trace.catalog_get();
        }
        let result = self.read_keyed(namespace, digest, mode);
        if let Some(trace) = &self.trace {
            trace.catalog_lookup_duration(started.elapsed());
        }
        result
    }

    fn read_keyed(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        mode: ReadMode,
    ) -> Result<Option<Found>, CatalogError> {
        let bulk_hit = match (self.bulk.as_mut(), mode) {
            (None, _) => None,
            (Some(bulk), ReadMode::Payload { max }) => bulk
                .segment
                .lookup_bounded(namespace, digest, max)?
                .map(|(kind, bytes)| Found::Payload { kind, bytes }),
            (Some(bulk), ReadMode::Length) => bulk
                .segment
                .logical_length(namespace, digest)
                .map(Found::Length),
        };
        if let Some(found) = bulk_hit {
            if let Some(trace) = &self.trace {
                trace.catalog_bulk_hit();
            }
            return Ok(Some(found));
        }
        if self.append_in_flight {
            return self.read_beside_append(namespace, digest, mode);
        }
        for attempt in 0..2 {
            let location = (|| -> Result<Option<LocatorRow>, CatalogError> {
                let mut lookup = self.index.prepare_cached(LOOKUP)?;
                Ok(lookup
                    .query_row(params![namespace, digest.as_slice()], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })
                    .optional()?)
            })();
            if let Ok(location) = location {
                if location.is_none()
                    && self
                        .lookup_authority(namespace, digest, None, mode)?
                        .is_some()
                {
                    if attempt == 0 {
                        self.rebuild_index()?;
                        continue;
                    }
                    return Err(CatalogError::Corrupt("locator misses authoritative object"));
                }
                match self.read_authoritative_location(namespace, digest, location.as_ref(), mode) {
                    Ok(value) => {
                        if value.is_some()
                            && let Some(trace) = &self.trace
                        {
                            trace.catalog_locator_hit();
                        }
                        return Ok(value);
                    }
                    Err(CatalogError::Corrupt(_)) | Err(CatalogError::Database(_))
                        if attempt == 0 => {}
                    Err(error) => return Err(error),
                }
            } else if attempt != 0 {
                return location.map(|_| None);
            }
            self.rebuild_index()?;
        }
        unreachable!()
    }

    /// A keyed read while an [`AppendJob`] runs. The locator may already hold rows
    /// of the records being appended, which the active segment has not published:
    /// they read as absent, as they are until the append finishes. Where the
    /// normal path would repair the locator, this returns
    /// [`CatalogError::AppendInFlight`] instead.
    fn read_beside_append(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        mode: ReadMode,
    ) -> Result<Option<Found>, CatalogError> {
        let found = |authority: Authority| match authority.payload {
            Some(bytes) => Found::Payload {
                kind: authority.entry.kind,
                bytes,
            },
            None => Found::Length(authority.entry.logical_length),
        };
        if let Some(authority) =
            self.lookup_authority(namespace, digest, Some(ACTIVE_FILE), mode)?
        {
            return Ok(Some(found(authority)));
        }
        let location = self
            .index
            .prepare_cached(LOOKUP)
            .and_then(|mut lookup| {
                lookup
                    .query_row(params![namespace, digest.as_slice()], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })
                    .optional()
            })
            .map_err(|_| CatalogError::AppendInFlight)?;
        match location {
            Some(location) if location.5 != ACTIVE_FILE => {
                match self.read_authoritative_location(namespace, digest, Some(&location), mode) {
                    Err(CatalogError::Corrupt(_) | CatalogError::Database(_)) => {
                        Err(CatalogError::AppendInFlight)
                    }
                    result => result,
                }
            }
            // No row, or a row of a record still being appended. Either way the
            // key is absent unless a sealed segment holds it, which would mean
            // the locator is stale.
            _ => match self.lookup_authority(namespace, digest, None, mode)? {
                Some(_) => Err(CatalogError::AppendInFlight),
                None => Ok(None),
            },
        }
    }

    fn read_authoritative_location(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        locator: Option<&LocatorRow>,
        mode: ReadMode,
    ) -> Result<Option<Found>, CatalogError> {
        let Some((kind, offset, codec, stored_length, logical_length, source)) = locator else {
            // The locator checkpoint is advanced in the same SQLite transaction as
            // its rows. Once it matches the authenticated catalog and active heads,
            // a miss is authoritative derived state; scanning and reauthenticating
            // every sealed pack here makes new-object ingestion quadratic after the
            // first rollover.
            return Ok(None);
        };
        if *stored_length > MAX_SEGMENT_PAYLOAD_BYTES {
            return Err(CatalogError::Corrupt(
                "locator stored length exceeds segment limit",
            ));
        }
        if let ReadMode::Payload { max } = mode
            && *logical_length > max as u64
        {
            return Err(CatalogError::Segment(SegmentError::ReadLimit {
                minimum: usize::try_from(*logical_length).unwrap_or(usize::MAX),
            }));
        }
        let Some(Authority {
            entry,
            payload: bytes,
        }) = self.lookup_authority(namespace, digest, Some(source), mode)?
        else {
            return Err(CatalogError::Corrupt(
                "locator source or key is not authoritative",
            ));
        };
        if entry.kind != *kind
            || entry.offset != *offset
            || entry.codec != *codec
            || entry.stored_length != *stored_length
            || entry.logical_length != *logical_length
        {
            return Err(CatalogError::Corrupt(
                "locator disagrees with authoritative index",
            ));
        }
        Ok(Some(match bytes {
            Some(bytes) => {
                verify_logical(&entry, &bytes)?;
                Found::Payload { kind: *kind, bytes }
            }
            None => Found::Length(entry.logical_length),
        }))
    }

    /// The authenticated index entry for a key, with its payload only in
    /// [`ReadMode::Payload`].
    fn lookup_authority(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        requested_source: Option<&str>,
        mode: ReadMode,
    ) -> Result<Option<Authority>, CatalogError> {
        if requested_source.is_none_or(|source| source == ACTIVE_FILE) {
            let found = match mode {
                ReadMode::Payload { max } => self
                    .active
                    .lookup_entry_bounded(namespace, digest, max)?
                    .map(|(entry, bytes)| Authority {
                        entry,
                        payload: Some(bytes),
                    }),
                ReadMode::Length => self.active.entry(namespace, digest).map(|entry| Authority {
                    entry: entry.clone(),
                    payload: None,
                }),
            };
            if let Some(found) = found {
                return Ok(Some(found));
            }
            if requested_source.is_some() {
                return Ok(None);
            }
        }

        let requested_index = match requested_source {
            Some(path) => Some(
                self.sealed_sources
                    .iter()
                    .position(|source| source.path == path)
                    .ok_or(CatalogError::Corrupt("non-canonical locator source path"))?,
            ),
            None => None,
        };
        let start = requested_index.unwrap_or(0);
        let end = requested_index.map_or(self.sealed_sources.len(), |index| index + 1);
        for index in start..end {
            let source = &self.sealed_sources[index];
            if !self.sealed_readers.contains_key(&source.segment) {
                let file = File::open(self.root.join(&source.path))?;
                if file.metadata()?.len() != source.stored_length {
                    return Err(CatalogError::Corrupt("authoritative source length"));
                }
                let reader = SegmentReader::open(file)?;
                if reader.id().0 != source.segment {
                    return Err(CatalogError::Corrupt(
                        "segment identity disagrees with catalog",
                    ));
                }
                #[cfg(test)]
                {
                    self.sealed_authentications += 1;
                }
                self.sealed_readers.insert(source.segment, reader);
            }
            let reader = self
                .sealed_readers
                .get_mut(&source.segment)
                .expect("reader was inserted");
            let found = match mode {
                ReadMode::Payload { max } => reader
                    .lookup_entry_bounded(namespace, digest, max)?
                    .map(|(entry, bytes)| Authority {
                        entry: entry.clone(),
                        payload: Some(bytes),
                    }),
                ReadMode::Length => reader.entry(namespace, digest).map(|entry| Authority {
                    entry: entry.clone(),
                    payload: None,
                }),
            };
            if let Some(found) = found {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn seal_active(&mut self) -> Result<(), CatalogError> {
        if self.active.state().object_count == 0 {
            return Ok(());
        }
        // From this point onward a returned filesystem, sync, publication, or
        // derived-index error may have left durable state behind.
        self.poisoned = true;
        let generation = self
            .generation
            .map_or(Some(0), |value| value.checked_add(1))
            .ok_or(CatalogError::Corrupt("generation overflow"))?;

        // Rebuild the segment with block compression via SegmentBuilder instead
        // of raw-copying the active file. This ensures every sealed segment
        // benefits from block compression of small structural records.
        let sealed = self.rebuild_active_as_block_compressed()?;

        let segment_name = format!("{}-0-{}{}", generation, hex(&sealed.id.0), SEGMENT_SUFFIX);
        let segment_path = self.segments.join(&segment_name);
        if !segment_path.exists() {
            let mut temporary = NamedTempFile::new_in(&self.segments)?;
            let mut source = sealed
                .file
                .reopen()
                .map_err(|e| CatalogError::Segment(SegmentError::Io(e)))?;
            io::copy(&mut source, &mut temporary)?;
            self.durability.sync_file(temporary.as_file())?;
            if let Err(error) = temporary.persist_noclobber(&segment_path)
                && error.error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error.error.into());
            }
        }
        let reader = SegmentReader::open(File::open(&segment_path)?)?;
        self.durability.sync_directory(&self.segments)?;

        let superseded = self.generation.zip(self.head);
        let digest =
            self.publish_appended_authority(generation, sealed.id.0, sealed.metadata.total_length)?;
        write_catalog_head(
            &self.root,
            &self.root.join(CATALOG_HEAD_FILE),
            Some((generation, digest)),
            self.durability,
        )?;
        self.generation = Some(generation);
        self.head = Some(digest);
        let sealed_source = format!("segments/{segment_name}");
        self.sealed_sources.push(SealedSource {
            path: sealed_source.clone(),
            generation,
            sequence: 0,
            segment: sealed.id.0,
            stored_length: sealed.metadata.total_length,
        });
        self.sealed_readers.insert(sealed.id.0, reader);
        #[cfg(test)]
        {
            self.sealed_authentications += 1;
        }
        // Keep this handle poisoned until replacement and derived-state repair
        // both complete, so every returned error requires reopen/recovery.
        // One source-row update repoints every locator entry in the sealed pack.
        #[cfg(test)]
        if self.seal_failpoint == Some(SealFailpoint::SqliteAfterHead) {
            return Err(CatalogError::Corrupt(
                "test SQLite failure after catalog head",
            ));
        }
        let transaction = self.index.transaction()?;
        // The old locator rows pointed at ACTIVE_FILE; drop them and rebuild
        // from the fresh sealed reader since the block-compressed segment has
        // different offsets/codecs than the per-record active file.
        transaction.execute(
            "DELETE FROM locator WHERE source_id IN \
             (SELECT id FROM physical_source WHERE path=?1)",
            params![ACTIVE_FILE],
        )?;
        transaction.execute(
            "DELETE FROM physical_source WHERE path=?1",
            params![ACTIVE_FILE],
        )?;
        if let Some(reader) = self.sealed_readers.get(&sealed.id.0) {
            insert_source_entries(&transaction, &sealed_source, reader.authenticated_entries())?;
        }
        transaction.commit()?;
        #[cfg(test)]
        let fail_during_retirement = self.seal_failpoint == Some(SealFailpoint::DuringRetirement);
        #[cfg(not(test))]
        let fail_during_retirement = false;
        retire_active(
            &self.root,
            &mut self.active,
            self.durability,
            fail_during_retirement,
        )?;
        self.index_active_and_checkpoint()?;
        remove_superseded_catalog(&self.catalogs, superseded, self.durability)?;
        self.poisoned = false;
        Ok(())
    }

    /// Reads all entries from the active segment and rebuilds them through
    /// `SegmentBuilder`, which applies block compression to small records.
    fn rebuild_active_as_block_compressed(&mut self) -> Result<SealedSegment, CatalogError> {
        let entries = self.active.entries().to_vec();
        let mut builder = SegmentBuilder::new(&self.segments)?;
        for entry in &entries {
            let (_, bytes) = self
                .active
                .lookup_entry_bounded(entry.namespace, entry.digest, usize::MAX)?
                .ok_or(CatalogError::Corrupt(
                    "active segment entry missing during seal rebuild",
                ))?;
            let record = SegmentRecord {
                namespace: entry.namespace,
                kind: entry.kind,
                digest: entry.digest,
                bytes: &bytes,
            };
            builder.push_prepared(prepare_record(CheckedRecord::check(record)?)?)?;
        }
        Ok(builder.seal()?)
    }

    fn publish_appended_authority(
        &self,
        generation: u64,
        segment: [u8; 32],
        stored_length: u64,
    ) -> Result<[u8; 32], CatalogError> {
        let mut segments = self
            .sealed_sources
            .iter()
            .map(|source| PhysicalSegmentDescriptor {
                generation: source.generation,
                sequence: source.sequence,
                segment: source.segment,
                stored_length: source.stored_length,
            })
            .collect::<Vec<_>>();
        segments.push(PhysicalSegmentDescriptor {
            generation,
            sequence: 0,
            segment,
            stored_length,
        });
        publish_physical_snapshot(
            &self.root,
            &self.catalogs,
            &PhysicalCatalogSnapshot {
                generation,
                segments,
                content: self.content_generation,
            },
            self.durability,
        )
    }

    /// Returns the complete published physical authority without sealing or
    /// otherwise publishing the active segment. Repack planning is a read-only
    /// preflight; execution binds this exact active state before doing work.
    pub(crate) fn physical_authority(&self) -> Result<CatalogPhysicalAuthority, CatalogError> {
        if self.durability != Durability::Durable || self.bulk.is_some() || self.poisoned {
            return Err(CatalogError::RepackUnsupported);
        }
        Ok(CatalogPhysicalAuthority {
            head: self.generation.zip(self.head),
            content: self.content_generation,
            active: self.active.state(),
        })
    }

    pub(crate) fn physical_stats(&mut self) -> Result<(usize, u64, u64), CatalogError> {
        self.load_sealed_readers()?;
        let sealed_bytes = self.sealed_sources.iter().try_fold(0_u64, |sum, source| {
            sum.checked_add(source.stored_length)
                .ok_or(CatalogError::Corrupt("physical byte count overflow"))
        })?;
        let bytes = sealed_bytes
            .checked_add(self.active.state().total_length)
            .ok_or(CatalogError::Corrupt("physical byte count overflow"))?;
        let sealed_records = self
            .sealed_readers
            .values()
            .try_fold(0_u64, |sum, reader| {
                let count = u64::try_from(reader.authenticated_entries().len())
                    .map_err(|_| CatalogError::Corrupt("record count overflow"))?;
                sum.checked_add(count)
                    .ok_or(CatalogError::Corrupt("record count overflow"))
            })?;
        let records = sealed_records
            .checked_add(self.active.state().object_count)
            .ok_or(CatalogError::Corrupt("record count overflow"))?;
        let segments = self
            .sealed_sources
            .len()
            .checked_add(usize::from(self.active.state().object_count != 0))
            .ok_or(CatalogError::Corrupt("segment count overflow"))?;
        Ok((segments, bytes, records))
    }

    pub(crate) fn content_generation(&self) -> PhysicalContentGeneration {
        self.content_generation
    }

    pub(crate) fn packed_content_ids(
        &mut self,
    ) -> Result<Vec<wheat_core::ContentId>, CatalogError> {
        self.load_sealed_readers()?;
        let mut ids = Vec::new();
        for source in &self.sealed_sources {
            let reader = self
                .sealed_readers
                .get(&source.segment)
                .ok_or(CatalogError::Corrupt(
                    "sealed reader missing during inventory",
                ))?;
            ids.extend(
                reader
                    .authenticated_entries()
                    .iter()
                    .filter(|entry| entry.namespace == super::segment::CONTENT_NAMESPACE)
                    .map(|entry| wheat_core::ContentId::from_digest(entry.digest)),
            );
        }
        ids.extend(
            self.active
                .entries()
                .iter()
                .filter(|entry| entry.namespace == super::segment::CONTENT_NAMESPACE)
                .map(|entry| wheat_core::ContentId::from_digest(entry.digest)),
        );
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    pub(super) fn repack_structural(
        &mut self,
        options: CatalogRepackOptions,
        replacement_content: PhysicalContentGeneration,
        // The store supplies this only after authenticating the generation pack.
        // The catalog independently checks that it covers every segment content ID.
        validated_content: Option<ValidatedContentGeneration<'_>>,
        expected_authority: CatalogPhysicalAuthority,
        metadata: MetadataRepackInput<'_>,
    ) -> Result<CatalogRepackReport, CatalogError> {
        if self.durability != Durability::Durable || self.bulk.is_some() || self.poisoned {
            return Err(CatalogError::RepackUnsupported);
        }
        if self.physical_authority()? != expected_authority {
            return Err(CatalogError::RepackStaleAuthority);
        }
        // Capacity admission has happened. Seal now, as the first publication in
        // execution, and continue only if it was the exact active state planned.
        self.seal_active()?;
        self.load_sealed_readers()?;

        let generation_before = self.generation.unwrap_or(0);
        let segments_before = self.sealed_sources.len();
        let physical_bytes_before = self.sealed_sources.iter().try_fold(0_u64, |sum, source| {
            sum.checked_add(source.stored_length)
                .ok_or(CatalogError::Corrupt("physical byte count overflow"))
        })?;
        // Inventory only authenticated keys and source references. Record bodies
        // are read and released one at a time below; a million-record repack must
        // never retain a million Vec payloads.
        let mut inventory = Vec::new();
        for source in &self.sealed_sources {
            let entries = self
                .sealed_readers
                .get(&source.segment)
                .ok_or(CatalogError::Corrupt("sealed reader missing during repack"))?
                .authenticated_entries();
            let next_len = inventory
                .len()
                .checked_add(entries.len())
                .ok_or(CatalogError::Corrupt("inventory count overflow"))?;
            if next_len > options.max_records {
                return Err(CatalogError::RepackInventoryLimit {
                    limit: options.max_records,
                });
            }
            inventory.extend(entries.iter().map(|entry| RepackInventoryEntry {
                source: RecordSource::Segment(source.segment),
                namespace: entry.namespace,
                kind: entry.kind,
                digest: entry.digest,
                logical_length: entry.logical_length,
            }));
        }
        let target = options
            .target_segment_bytes
            .clamp(1, MAX_SEGMENT_PAYLOAD_BYTES);
        // A loose object moves into segments only if it fits one segment on
        // its own; anything larger keeps its loose file.
        let absorbed_loose = metadata
            .loose
            .iter()
            .filter(|record| record.logical_length <= target)
            .map(|record| record.digest)
            .collect::<Vec<_>>();
        if inventory.len().saturating_add(absorbed_loose.len()) > options.max_records {
            return Err(CatalogError::RepackInventoryLimit {
                limit: options.max_records,
            });
        }
        inventory.extend(
            metadata
                .loose
                .iter()
                .filter(|record| record.logical_length <= target)
                .map(|record| RepackInventoryEntry {
                    source: RecordSource::Loose,
                    namespace: METADATA_NAMESPACE,
                    kind: record.kind,
                    digest: record.digest,
                    logical_length: record.logical_length,
                }),
        );
        inventory.sort_unstable_by_key(|record| (record.namespace, record.digest, record.source));
        inventory.dedup_by(|right, left| {
            if (left.namespace, left.digest) != (right.namespace, right.digest) {
                return false;
            }
            if left.kind != right.kind || left.logical_length != right.logical_length {
                return false;
            }
            true
        });
        if inventory
            .windows(2)
            .any(|pair| (pair[0].namespace, pair[0].digest) == (pair[1].namespace, pair[1].digest))
        {
            return Err(CatalogError::Corrupt(
                "duplicate key has conflicting authenticated metadata",
            ));
        }
        // A generation token does not authorize removing CONTENT_NAMESPACE.
        // Only the store can furnish this manifest after a complete pack scan;
        // independently require coverage for every authenticated segment content ID.
        let may_retire_content = if replacement_content == no_content_generation() {
            false
        } else {
            let manifest = validated_content
                .and_then(|capability| capability.manifest_for(replacement_content))
                .ok_or(CatalogError::Corrupt(
                    "content retirement requires a validated generation inventory",
                ))?;
            if manifest.generation != replacement_content.generation {
                return Err(CatalogError::Corrupt(
                    "validated generation inventory names another generation",
                ));
            }
            if inventory
                .iter()
                .filter(|record| record.namespace == super::segment::CONTENT_NAMESPACE)
                .any(|record| {
                    manifest
                        .entries
                        .binary_search_by_key(
                            &wheat_core::ContentId::from_digest(record.digest),
                            |entry| entry.id,
                        )
                        .is_err()
                })
            {
                return Err(CatalogError::Corrupt(
                    "validated generation inventory does not cover segment content",
                ));
            }
            true
        };
        if may_retire_content {
            inventory.retain(|record| record.namespace != super::segment::CONTENT_NAMESPACE);
        }
        let structural_records = inventory.iter().try_fold(0_u64, |count, record| {
            if record.namespace == super::segment::METADATA_NAMESPACE {
                count
                    .checked_add(1)
                    .ok_or(CatalogError::Corrupt("record count overflow"))
            } else {
                Ok(count)
            }
        })?;

        let next_generation = generation_before
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("generation overflow"))?;
        let (order, planned_bases) = delta_layout(&inventory, metadata.bases)?;
        let mut is_base = vec![false; inventory.len()];
        for base in planned_bases.iter().flatten() {
            is_base[*base] = true;
        }
        let mut replacements = Vec::new();
        let mut source_body_reads = 0u64;
        let mut max_resident_bodies = 0u64;
        let mut delta_records = 0u64;
        let mut max_delta_depth = 0u8;
        let mut builder = SegmentBuilder::new(&self.segments)?;
        let mut builder_is_empty = true;
        // The previously written record's body, kept only when a planned
        // delta uses it as a base. In base-first layout that delta usually
        // comes next, so a base is rarely read twice.
        let mut previous: Option<([u8; 32], Vec<u8>)> = None;
        for index in order {
            let record = inventory[index];
            let bytes = self.read_repack_body(record, metadata.read_loose)?;
            source_body_reads = source_body_reads
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("source body read count overflow"))?;
            let logical = SegmentRecord {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                bytes: &bytes,
            };
            let whole = prepare_record(CheckedRecord::check(logical)?)?;
            let mut resident = if previous.is_some() { 2 } else { 1 };
            // A planned base is usable only while it is in the open segment
            // with room for one more link.
            let usable_base = planned_bases[index].filter(|base| {
                builder
                    .metadata_depth(inventory[*base].digest)
                    .is_some_and(|depth| depth < wheat_core::SEGMENT_MAX_DELTA_DEPTH)
            });
            let mut delta = None;
            if let Some(base) = usable_base {
                let base = inventory[base];
                let base_bytes = match &previous {
                    Some((digest, body)) if *digest == base.digest => Cow::Borrowed(body),
                    _ => {
                        source_body_reads = source_body_reads
                            .checked_add(1)
                            .ok_or(CatalogError::Corrupt("source body read count overflow"))?;
                        resident += 1;
                        Cow::Owned(self.read_repack_body(base, metadata.read_loose)?)
                    }
                };
                delta = prepare_delta_record(logical, base.digest, &base_bytes)?
                    .filter(|delta| delta.physical_cost() < whole.stored_length());
            }
            max_resident_bodies = max_resident_bodies.max(resident);
            let cost = delta
                .as_ref()
                .map_or(whole.stored_length(), |delta| delta.physical_cost());
            if !builder_is_empty
                && builder
                    .projected_payload_bytes()
                    .checked_add(cost)
                    .is_none_or(|bytes| bytes > target)
            {
                // The chain is cut here: this record's base stays behind in
                // the sealed segment, so it starts the next one whole.
                replacements.push(builder.seal()?);
                builder = SegmentBuilder::new(&self.segments)?;
                delta = None;
            }
            match delta {
                Some(delta) => {
                    max_delta_depth = max_delta_depth.max(builder.push_delta(delta)?);
                    delta_records += 1;
                }
                None => builder.push_prepared(whole)?,
            }
            builder_is_empty = false;
            previous = is_base[index].then_some((record.digest, bytes));
        }
        if !builder_is_empty {
            replacements.push(builder.seal()?);
        }

        let proposed_ids = replacements
            .iter()
            .map(|segment| segment.id.0)
            .collect::<Vec<_>>();
        let current_ids = self
            .sealed_sources
            .iter()
            .map(|source| source.segment)
            .collect::<Vec<_>>();
        if proposed_ids == current_ids && replacement_content == self.content_generation {
            return Ok(CatalogRepackReport {
                no_op: true,
                generation_before,
                generation_after: generation_before,
                segments_before,
                segments_after: segments_before,
                physical_bytes_before,
                physical_bytes_after: physical_bytes_before,
                structural_records,
                source_body_reads: 0,
                max_resident_bodies: 0,
                bytes_written: 0,
                bytes_retired: 0,
                // The proposal equals the current segments, so its layout
                // statistics describe them too.
                delta_records,
                max_delta_depth,
                absorbed_loose,
            });
        }

        self.poisoned = true;
        let mut descriptors = Vec::with_capacity(replacements.len());
        let mut new_sources = Vec::with_capacity(replacements.len());
        let mut new_readers = HashMap::with_capacity(replacements.len());
        let mut physical_bytes_after = 0_u64;
        for (sequence, replacement) in replacements.into_iter().enumerate() {
            let sequence = u64::try_from(sequence)
                .map_err(|_| CatalogError::Corrupt("segment sequence overflow"))?;
            let name = format!(
                "{next_generation}-{sequence}-{}{}",
                hex(&replacement.id.0),
                SEGMENT_SUFFIX
            );
            let path = self.segments.join(&name);
            replacement
                .file
                .persist_noclobber(&path)
                .map_err(|error| error.error)?;
            let file = File::open(&path)?;
            self.durability.sync_file(&file)?;
            let reader = SegmentReader::open(file)?;
            if reader.id() != replacement.id {
                return Err(CatalogError::Corrupt("replacement segment identity"));
            }
            physical_bytes_after = physical_bytes_after
                .checked_add(replacement.metadata.total_length)
                .ok_or(CatalogError::Corrupt("physical byte count overflow"))?;
            descriptors.push(PhysicalSegmentDescriptor {
                generation: next_generation,
                sequence,
                segment: replacement.id.0,
                stored_length: replacement.metadata.total_length,
            });
            new_sources.push(SealedSource {
                path: format!("segments/{name}"),
                generation: next_generation,
                sequence,
                segment: replacement.id.0,
                stored_length: replacement.metadata.total_length,
            });
            new_readers.insert(replacement.id.0, reader);
        }
        self.durability.sync_directory(&self.segments)?;
        let snapshot = PhysicalCatalogSnapshot {
            generation: next_generation,
            segments: descriptors,
            content: replacement_content,
        };
        let digest =
            publish_physical_snapshot(&self.root, &self.catalogs, &snapshot, self.durability)?;
        write_repack_intent(
            &self.root,
            self.generation.zip(self.head),
            (next_generation, digest),
            self.durability,
        )?;
        #[cfg(test)]
        if self.repack_failpoint == Some(RepackFailpoint::AfterIntent) {
            return Err(CatalogError::Corrupt("test failure after repack intent"));
        }
        write_catalog_head(
            &self.root,
            &self.root.join(CATALOG_HEAD_FILE),
            Some((next_generation, digest)),
            self.durability,
        )?;
        #[cfg(test)]
        if self.repack_failpoint == Some(RepackFailpoint::AfterHead) {
            return Err(CatalogError::Corrupt("test failure after repack head"));
        }

        self.generation = Some(next_generation);
        self.head = Some(digest);
        self.content_generation = replacement_content;
        self.sealed_readers = new_readers;
        let old_sources = std::mem::replace(&mut self.sealed_sources, new_sources);
        self.rebuild_index()?;
        for source in old_sources {
            let path = self.root.join(source.path);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        self.durability.sync_directory(&self.segments)?;
        remove_unreachable_publications(
            &self.segments,
            &self.catalogs,
            self.generation.zip(self.head),
            Some(&snapshot),
            self.durability,
        )?;
        remove_durable_file(
            &self.root,
            &self.root.join(REPACK_INTENT_FILE),
            self.durability,
        )?;
        self.poisoned = false;
        Ok(CatalogRepackReport {
            no_op: false,
            generation_before,
            generation_after: next_generation,
            segments_before,
            segments_after: self.sealed_sources.len(),
            physical_bytes_before,
            physical_bytes_after,
            structural_records,
            source_body_reads,
            max_resident_bodies,
            bytes_written: physical_bytes_after,
            bytes_retired: physical_bytes_before,
            delta_records,
            max_delta_depth,
            absorbed_loose,
        })
    }

    /// Reads one inventory record's verified logical bytes from its source.
    fn read_repack_body(
        &mut self,
        record: RepackInventoryEntry,
        read_loose: &mut dyn FnMut([u8; 32]) -> Result<Vec<u8>, CatalogError>,
    ) -> Result<Vec<u8>, CatalogError> {
        match record.source {
            RecordSource::Segment(segment) => {
                let reader = self
                    .sealed_readers
                    .get_mut(&segment)
                    .ok_or(CatalogError::Corrupt("repack source reader missing"))?;
                let (_, bytes) = reader
                    .lookup_entry_bounded(record.namespace, record.digest, usize::MAX)?
                    .ok_or(CatalogError::Corrupt(
                        "authenticated inventory entry missing",
                    ))?;
                reader.clear_block_cache();
                Ok(bytes)
            }
            RecordSource::Loose => {
                let bytes = read_loose(record.digest)?;
                // The loose reader verified identity; the inventory's kind and
                // length came from the same verified bytes at planning time.
                if bytes.len() as u64 != record.logical_length {
                    return Err(CatalogError::Corrupt(
                        "loose metadata changed since planning",
                    ));
                }
                Ok(bytes)
            }
        }
    }

    fn rebuild_index(&mut self) -> Result<(), CatalogError> {
        self.load_sealed_readers()?;
        let catalog_head = self.generation.zip(self.head);
        let transaction = self.index.transaction()?;
        transaction.execute("DELETE FROM locator", [])?;
        transaction.execute("DELETE FROM physical_source", [])?;
        for source in &self.sealed_sources {
            let reader = self
                .sealed_readers
                .get(&source.segment)
                .ok_or(CatalogError::Corrupt(
                    "sealed descriptor lacks cached reader",
                ))?;
            insert_source_entries(&transaction, &source.path, reader.authenticated_entries())?;
        }
        insert_source_entries(&transaction, ACTIVE_FILE, self.active.entries())?;
        let locator_rows = locator_count(&transaction)?;
        write_checkpoint(
            &transaction,
            catalog_head,
            self.active.state(),
            locator_rows,
        )?;
        transaction.commit()?;
        self.locator_rows = locator_rows;
        Ok(())
    }

    fn index_active_and_checkpoint(&mut self) -> Result<(), CatalogError> {
        let entries = self.active.entries();
        let transaction = self.index.transaction()?;
        insert_source_entries(&transaction, ACTIVE_FILE, entries)?;
        write_checkpoint(
            &transaction,
            self.generation.zip(self.head),
            self.active.state(),
            self.locator_rows,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Closes the derived SQLite index explicitly before releasing the packed-store lock.
    ///
    /// `rusqlite::Connection` can report errors while closing (for example, from unfinished
    /// statements), which its `Drop` implementation cannot surface.
    pub(crate) fn close(mut self) -> Result<(), CatalogError> {
        #[cfg(test)]
        if self.close_failpoint == Some(CloseFailpoint::BeforeFinalize) {
            return Err(CatalogError::Corrupt(
                "test failure before deferred bulk finalization",
            ));
        }
        self.finalize_bulk()?;
        let Self {
            root,
            segments,
            catalogs,
            _lock,
            index,
            append_index,
            append_in_flight: _,
            generation: _,
            head: _,
            content_generation: _,
            sealed_sources: _,
            sealed_readers: _,
            active,
            bulk: _,
            durability: _,
            active_target_bytes: _,
            max_segment_entries: _,
            max_segment_payload_bytes: _,
            locator_rows: _,
            poisoned: _,
            trace: _,
            #[cfg(test)]
                sealed_authentications: _,
            #[cfg(test)]
                seal_failpoint: _,
            #[cfg(test)]
                append_failpoint: _,
            #[cfg(test)]
                append_pause: _,
            #[cfg(test)]
                deferred_failpoint: _,
            #[cfg(test)]
                close_failpoint: _,
            #[cfg(test)]
                repack_failpoint: _,
        } = self;

        // `index` closes last: the last connection to close folds the write-ahead
        // log into the database.
        let close = |connection: Connection| {
            connection.close().map_err(|(connection, error)| {
                drop(connection);
                CatalogError::Database(error)
            })
        };
        let append_closed = append_index.map_or(Ok(()), close);
        let index_closed = close(index);
        drop(active);
        drop(_lock);
        drop(catalogs);
        drop(segments);
        drop(root);
        append_closed.and(index_closed)
    }

    fn recount_locator_rows(&self) -> Result<u64, CatalogError> {
        if let Some(trace) = &self.trace {
            trace.locator_recount();
        }
        let started = Instant::now();
        let count = locator_count(&self.index);
        if let Some(trace) = &self.trace {
            trace.locator_recount_duration(started.elapsed());
        }
        count
    }

    fn load_sealed_readers(&mut self) -> Result<(), CatalogError> {
        for source in &self.sealed_sources {
            if self.sealed_readers.contains_key(&source.segment) {
                continue;
            }
            let file = File::open(self.root.join(&source.path))?;
            if file.metadata()?.len() != source.stored_length {
                return Err(CatalogError::Corrupt("authoritative source length"));
            }
            let reader = SegmentReader::open(file)?;
            if reader.id().0 != source.segment {
                return Err(CatalogError::Corrupt(
                    "segment identity disagrees with catalog",
                ));
            }
            self.sealed_readers.insert(source.segment, reader);
            #[cfg(test)]
            {
                self.sealed_authentications += 1;
            }
        }
        Ok(())
    }

    fn read_checkpoint(&self) -> Result<CatalogCheckpoint, CatalogError> {
        let row = self.index.query_row(
            "SELECT catalog_generation,catalog_digest,active_generation,active_id,\
             active_object_count,active_payload_bytes,active_total_length,locator_count \
             FROM checkpoint WHERE singleton=1",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )?;
        let catalog = match (row.0, row.1) {
            (None, None) => None,
            (Some(generation), Some(digest)) => Some((u64_value(generation)?, digest32(digest)?)),
            _ => return Err(CatalogError::Corrupt("locator catalog checkpoint")),
        };
        let active = ActivePublishedState {
            generation: u64_value(row.2)?,
            id: SegmentId(digest32(row.3)?),
            object_count: u64_value(row.4)?,
            payload_bytes: u64_value(row.5)?,
            total_length: u64_value(row.6)?,
        };
        Ok((catalog, active, u64_value(row.7)?))
    }
}

/// Orders `inventory` (sorted by key) so every planned delta base precedes its
/// dependents, and returns each record's planned base as an inventory index.
///
/// A base counts only when both records are metadata in this inventory. The
/// order is a depth-first walk from every record without a planned base, in
/// key order, visiting dependents in key order. A record whose base chain
/// cycles is reached from no such root; the first unvisited cycle member in key
/// order becomes a root instead and is written whole because its base has not
/// been written yet. Everything here depends only on the inventory and the
/// plan, so an unchanged store lays out identically and repack stays a no-op.
///
/// Dependents are kept in one flat CSR table rather than a list per base.
fn delta_layout(
    inventory: &[RepackInventoryEntry],
    bases: &BTreeMap<[u8; 32], [u8; 32]>,
) -> Result<(Vec<usize>, Vec<Option<usize>>), CatalogError> {
    let position = |digest: [u8; 32]| {
        inventory
            .binary_search_by_key(&(METADATA_NAMESPACE, digest), |record| {
                (record.namespace, record.digest)
            })
            .ok()
    };
    let planned = inventory
        .iter()
        .enumerate()
        .map(|(index, record)| {
            if record.namespace != METADATA_NAMESPACE {
                return None;
            }
            bases
                .get(&record.digest)
                .and_then(|base| position(*base))
                .filter(|base| *base != index)
        })
        .collect::<Vec<_>>();

    // CSR: dependents of base `b` are `targets[offsets[b]..offsets[b + 1]]`,
    // already in key order because targets are pushed in index order.
    let mut offsets = vec![0_usize; inventory.len() + 1];
    for base in planned.iter().flatten() {
        offsets[*base + 1] += 1;
    }
    for index in 0..inventory.len() {
        offsets[index + 1] += offsets[index];
    }
    let mut fill = offsets.clone();
    let mut targets = vec![0_usize; offsets[inventory.len()]];
    for (target, base) in planned.iter().enumerate() {
        if let Some(base) = base {
            targets[fill[*base]] = target;
            fill[*base] += 1;
        }
    }

    let mut order = Vec::with_capacity(inventory.len());
    let mut visited = vec![false; inventory.len()];
    let mut stack = Vec::new();
    let roots = (0..inventory.len())
        .filter(|index| planned[*index].is_none())
        .chain(0..inventory.len());
    for root in roots {
        if visited[root] {
            continue;
        }
        stack.push(root);
        while let Some(index) = stack.pop() {
            if visited[index] {
                continue;
            }
            visited[index] = true;
            order.push(index);
            // Reverse so the smallest dependent is visited first.
            stack.extend(
                targets[offsets[index]..offsets[index + 1]]
                    .iter()
                    .rev()
                    .filter(|target| !visited[**target]),
            );
        }
    }
    if order.len() != inventory.len() {
        return Err(CatalogError::Corrupt("delta layout lost a record"));
    }
    Ok((order, planned))
}

/// Opens the active segment at its newest verified frame, dropping an append a
/// crash interrupted, or creates it.
fn recover_active(root: &Path, durability: Durability) -> Result<ActiveSegment, CatalogError> {
    let active_path = root.join(ACTIVE_FILE);
    match ActiveSegment::recover(&active_path) {
        Ok((active, torn)) => {
            if torn {
                active.sync(durability)?;
            }
            return Ok(active);
        }
        Err(ActiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        // Only the first creation writes this path in place: retirement renames
        // a synced log over it. No append can have followed an unfinished header.
        Err(ActiveError::Unwritten) => fs::remove_file(&active_path)?,
        Err(error) => return Err(error.into()),
    }
    let active = ActiveSegment::create(&active_path, 0)?;
    active.sync(durability)?;
    durability.sync_directory(root)?;
    Ok(active)
}

/// The block-compressed replacement need not equal the active segment's byte
/// identity; publication authenticated the replacement before this step.
fn retire_active(
    root: &Path,
    active: &mut ActiveSegment,
    durability: Durability,
    fail_before_rename: bool,
) -> Result<(), CatalogError> {
    let next_generation = active
        .state()
        .generation
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("active generation overflow"))?;
    let temporary_path = root.join("active.segment.next");
    match fs::remove_file(&temporary_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut replacement = ActiveSegment::create(&temporary_path, next_generation)?;
    replacement.sync(durability)?;
    if fail_before_rename {
        return Err(CatalogError::Corrupt(
            "test failure during active retirement",
        ));
    }
    let active_path = root.join(ACTIVE_FILE);
    fs::rename(&temporary_path, &active_path)?;
    replacement.renamed(active_path);
    durability.sync_directory(root)?;
    *active = replacement;
    Ok(())
}

fn open_index(path: &Path, durability: Durability) -> Result<Connection, CatalogError> {
    let connection = connect_index(path, durability)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        connection.execute_batch(SCHEMA)?;
    } else if version != SCHEMA_VERSION {
        return Err(CatalogError::Corrupt("locator schema version"));
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(CatalogError::Corrupt("locator database integrity"));
    }
    Ok(connection)
}

/// A connection to the locator database, without the checks [`open_index`] makes.
/// Write-ahead logging lets one connection read while another commits.
fn connect_index(path: &Path, durability: Durability) -> Result<Connection, CatalogError> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // The locator is derived: an open whose checkpoint disagrees with the
    // segments rebuilds it. So commits need not survive a power loss, and in
    // write-ahead-log mode `NORMAL` makes them without a sync.
    connection.pragma_update(
        None,
        "synchronous",
        match durability {
            Durability::Durable => "NORMAL",
            Durability::Deferred => "OFF",
        },
    )?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -8192)?;
    Ok(connection)
}

fn remove_unpublished_bulk_files(
    segments: &Path,
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut removed = false;
    for entry in fs::read_dir(segments)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(BULK_FILE_PREFIX) {
            fs::remove_file(entry.path())?;
            removed = true;
        }
    }
    if removed {
        durability.sync_directory(segments)?;
    }
    Ok(())
}

/// Removes only authenticated immutable publications whose canonical names are
/// absent from the already-validated authoritative snapshot. Unknown,
/// non-canonical, and malformed entries are deliberately preserved.
fn remove_unreachable_publications(
    segments: &Path,
    catalogs: &Path,
    head: CatalogHead,
    authority: Option<&PhysicalCatalogSnapshot>,
    durability: Durability,
) -> Result<(), CatalogError> {
    let reachable_catalogs: HashSet<String> = head
        .map(|(generation, digest)| {
            [format!("{}-{}{}", generation, hex(&digest), CATALOG_SUFFIX)]
                .into_iter()
                .collect()
        })
        .unwrap_or_default();
    let reachable_segments: HashSet<String> = authority
        .map_or_else(Vec::new, authority_sources)
        .into_iter()
        .map(|source| source.path.trim_start_matches("segments/").to_owned())
        .collect();

    let mut removed_catalog = false;
    for entry in fs::read_dir(catalogs)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if reachable_catalogs.contains(&name) {
            continue;
        }
        let Some((generation, digest)) = parse_canonical_publication_name(&name, CATALOG_SUFFIX)
        else {
            continue;
        };
        let encoded = fs::read(entry.path())?;
        if blake3::hash(&encoded).as_bytes() != &digest {
            continue;
        }
        let valid_generation =
            decode_physical_catalog_snapshot(&encoded).map(|snapshot| snapshot.generation);
        if valid_generation != Ok(generation) {
            continue;
        }
        fs::remove_file(entry.path())?;
        removed_catalog = true;
    }
    if removed_catalog {
        durability.sync_directory(catalogs)?;
    }

    let mut removed_segment = false;
    for entry in fs::read_dir(segments)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if reachable_segments.contains(&name) {
            continue;
        }
        let Some((_generation, _sequence, segment_id)) = parse_canonical_segment_name(&name) else {
            continue;
        };
        let Ok(reader) = File::open(entry.path()).and_then(|file| {
            SegmentReader::open(file)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }) else {
            continue;
        };
        if reader.id().0 != segment_id {
            continue;
        }
        fs::remove_file(entry.path())?;
        removed_segment = true;
    }
    if removed_segment {
        durability.sync_directory(segments)?;
    }
    Ok(())
}

/// Parses `{generation}-{hex}{suffix}` (catalog and manifest publications).
fn parse_canonical_publication_name(name: &str, suffix: &str) -> Option<(u64, [u8; 32])> {
    let (generation_text, digest_text) = name.strip_suffix(suffix)?.split_once('-')?;
    Some((
        parse_canonical_decimal(generation_text)?,
        parse_lower_hex_digest(digest_text)?,
    ))
}

/// Parses `{generation}-{sequence}-{hex}.segment`, the only sealed-segment name.
fn parse_canonical_segment_name(name: &str) -> Option<(u64, u64, [u8; 32])> {
    let (generation_text, rest) = name.strip_suffix(SEGMENT_SUFFIX)?.split_once('-')?;
    let (sequence_text, digest_text) = rest.split_once('-')?;
    Some((
        parse_canonical_decimal(generation_text)?,
        parse_canonical_decimal(sequence_text)?,
        parse_lower_hex_digest(digest_text)?,
    ))
}

fn parse_canonical_decimal(text: &str) -> Option<u64> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

fn parse_lower_hex_digest(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut digest = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digest)
}

fn remove_index_files(path: &Path) -> Result<(), CatalogError> {
    let sidecar = |suffix: &str| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    for candidate in [
        path.to_path_buf(),
        sidecar("-journal"),
        sidecar("-wal"),
        sidecar("-shm"),
    ] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn insert_source_entries(
    transaction: &Transaction<'_>,
    source: &str,
    entries: &[SegmentIndexEntry],
) -> Result<(), CatalogError> {
    transaction.execute(
        "INSERT INTO physical_source(path) VALUES(?1) ON CONFLICT(path) DO NOTHING",
        [source],
    )?;
    let source_id: i64 = transaction.query_row(
        "SELECT id FROM physical_source WHERE path=?1",
        [source],
        |row| row.get(0),
    )?;
    let mut insert = transaction.prepare_cached(
        "INSERT INTO locator(namespace,digest,kind,offset,codec,stored_length,logical_length,source_id) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(namespace,digest) DO UPDATE SET \
         kind=excluded.kind,offset=excluded.offset,codec=excluded.codec,\
         stored_length=excluded.stored_length,logical_length=excluded.logical_length,\
         source_id=excluded.source_id",
    )?;
    for entry in entries {
        insert.execute(params![
            entry.namespace,
            entry.digest.as_slice(),
            entry.kind,
            entry.offset,
            entry.codec,
            entry.stored_length,
            entry.logical_length,
            source_id
        ])?;
    }
    Ok(())
}

/// Adds the locator rows of an append's new entries and moves the checkpoint to
/// the active segment's new `state`. Returns the new row count.
fn index_appended_entries(
    index: &mut Connection,
    entries: &[SegmentIndexEntry],
    catalog_head: CatalogHead,
    state: ActivePublishedState,
    locator_rows: u64,
) -> Result<u64, CatalogError> {
    let locator_rows = u64::try_from(entries.len())
        .ok()
        .and_then(|added| locator_rows.checked_add(added))
        .ok_or(CatalogError::Corrupt("locator count overflow"))?;
    let transaction = index.transaction()?;
    insert_source_entries(&transaction, ACTIVE_FILE, entries)?;
    write_checkpoint(&transaction, catalog_head, state, locator_rows)?;
    transaction.commit()?;
    Ok(locator_rows)
}

fn locator_count(connection: &Connection) -> Result<u64, CatalogError> {
    let count: i64 = connection.query_row("SELECT count(*) FROM locator", [], |row| row.get(0))?;
    u64_value(count)
}

fn write_checkpoint(
    transaction: &Transaction<'_>,
    catalog: Option<(u64, [u8; 32])>,
    active: ActivePublishedState,
    locator_rows: u64,
) -> Result<(), CatalogError> {
    let (generation, digest) = match catalog {
        Some((generation, digest)) => (Some(i64_value(generation)?), Some(digest)),
        None => (None, None),
    };
    let locator_count = i64_value(locator_rows)?;
    transaction.execute(
        "UPDATE checkpoint SET catalog_generation=?1,catalog_digest=?2,active_generation=?3,\
         active_id=?4,active_object_count=?5,active_payload_bytes=?6,active_total_length=?7,\
         locator_count=?8 WHERE singleton=1",
        params![
            generation,
            digest.as_ref().map(|value| value.as_slice()),
            i64_value(active.generation)?,
            active.id.0.as_slice(),
            i64_value(active.object_count)?,
            i64_value(active.payload_bytes)?,
            i64_value(active.total_length)?,
            locator_count,
        ],
    )?;
    Ok(())
}

/// Each appended snapshot lists every sealed segment, so keeping superseded
/// snapshots until the next open would cost O(seals x segments) disk. Called
/// only after the new head is durable; a crash before this point leaves the
/// old snapshot for open-time cleanup of unreachable publications.
fn remove_superseded_catalog(
    catalogs: &Path,
    superseded: CatalogHead,
    durability: Durability,
) -> Result<(), CatalogError> {
    let Some((generation, digest)) = superseded else {
        return Ok(());
    };
    let path = catalogs.join(format!("{}-{}{}", generation, hex(&digest), CATALOG_SUFFIX));
    match fs::remove_file(path) {
        Ok(()) => durability.sync_directory(catalogs)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn publish_physical_snapshot(
    root: &Path,
    catalogs: &Path,
    snapshot: &PhysicalCatalogSnapshot,
    durability: Durability,
) -> Result<[u8; 32], CatalogError> {
    let encoded = encode_physical_catalog_snapshot(snapshot)?;
    publish_catalog_bytes(root, catalogs, snapshot.generation, &encoded, durability)
}

fn publish_catalog_bytes(
    root: &Path,
    catalogs: &Path,
    generation: u64,
    encoded: &[u8],
    durability: Durability,
) -> Result<[u8; 32], CatalogError> {
    let digest = *blake3::hash(encoded).as_bytes();
    let path = catalogs.join(format!("{}-{}.catalog", generation, hex(&digest)));
    let mut temporary = NamedTempFile::new_in(root)?;
    temporary.write_all(encoded)?;
    durability.sync_file(temporary.as_file())?;
    if let Err(error) = temporary.persist_noclobber(&path) {
        if error.error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error.error.into());
        }
        if fs::read(path)? != encoded {
            return Err(CatalogError::Corrupt("conflicting catalog retry"));
        }
    }
    durability.sync_directory(catalogs)?;
    Ok(digest)
}

fn read_catalog_head(path: &Path) -> Result<CatalogHead, CatalogError> {
    let head = decode_physical_catalog_head(&fs::read(path)?)?;
    Ok(head.map(catalog_head_pair))
}

fn write_catalog_head(
    root: &Path,
    path: &Path,
    head: CatalogHead,
    durability: Durability,
) -> Result<(), CatalogError> {
    let encoded = encode_physical_catalog_head(head.map(catalog_ref))?;
    atomic_write(root, path, &encoded, durability)
}

fn catalog_ref((generation, digest): (u64, [u8; 32])) -> PhysicalCatalogRef {
    PhysicalCatalogRef { generation, digest }
}

fn catalog_head_pair(head: PhysicalCatalogRef) -> (u64, [u8; 32]) {
    (head.generation, head.digest)
}

/// Reads the snapshot named by the durable head. `None` means a store that
/// has not sealed anything yet: no segments and no content generation.
fn read_catalog_authority(
    directory: &Path,
    head: CatalogHead,
) -> Result<Option<PhysicalCatalogSnapshot>, CatalogError> {
    let Some((generation, digest)) = head else {
        return Ok(None);
    };
    let encoded = read_catalog_bytes(directory, generation, digest)?;
    let snapshot = decode_physical_catalog_snapshot(&encoded)?;
    if snapshot.generation != generation {
        return Err(CatalogError::Corrupt("catalog snapshot generation"));
    }
    Ok(Some(snapshot))
}

fn authority_sources(snapshot: &PhysicalCatalogSnapshot) -> Vec<SealedSource> {
    snapshot
        .segments
        .iter()
        .map(|item| SealedSource {
            path: format!(
                "segments/{}-{}-{}{}",
                item.generation,
                item.sequence,
                hex(&item.segment),
                SEGMENT_SUFFIX
            ),
            generation: item.generation,
            sequence: item.sequence,
            segment: item.segment,
            stored_length: item.stored_length,
        })
        .collect()
}

/// The content authority before any repack has selected a content generation:
/// content still lives in segments and loose envelopes.
pub(super) fn no_content_generation() -> PhysicalContentGeneration {
    PhysicalContentGeneration {
        generation: 0,
        manifest: [0; 32],
    }
}

fn read_catalog_bytes(
    directory: &Path,
    generation: u64,
    digest: [u8; 32],
) -> Result<Vec<u8>, CatalogError> {
    let path = directory.join(format!("{}-{}{}", generation, hex(&digest), CATALOG_SUFFIX));
    let encoded = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CatalogError::Corrupt("catalog head references missing record")
        } else {
            CatalogError::Io(error)
        }
    })?;
    if blake3::hash(&encoded).as_bytes() != &digest {
        return Err(CatalogError::Corrupt("catalog record identity"));
    }
    Ok(encoded)
}

/// Recovers the head when `catalog.head` is missing: the unique highest
/// generation whose authenticated snapshot names only segment files that are
/// present at their recorded lengths. Orphans (e.g. an interrupted repack's
/// snapshot whose segments were already removed) are skipped.
fn latest_catalog_head(
    directory: &Path,
    segments: &Path,
) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some((generation, digest)) = name
            .to_str()
            .and_then(|name| parse_canonical_publication_name(name, CATALOG_SUFFIX))
        else {
            continue;
        };
        let encoded = fs::read(entry.path())?;
        if blake3::hash(&encoded).as_bytes() != &digest {
            continue;
        }
        let Ok(snapshot) = decode_physical_catalog_snapshot(&encoded) else {
            continue;
        };
        if snapshot.generation != generation {
            continue;
        }
        let mut complete = true;
        for source in authority_sources(&snapshot) {
            let name = source.path.trim_start_matches("segments/");
            match fs::metadata(segments.join(name)) {
                Ok(metadata) if metadata.is_file() && metadata.len() == source.stored_length => {}
                Ok(_) => complete = false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => complete = false,
                Err(error) => return Err(error.into()),
            }
            if !complete {
                break;
            }
        }
        if complete {
            candidates.push((generation, digest));
        }
    }
    let Some(max_generation) = candidates.iter().map(|candidate| candidate.0).max() else {
        return Ok(None);
    };
    let mut latest = candidates
        .into_iter()
        .filter(|candidate| candidate.0 == max_generation);
    let result = latest.next().unwrap();
    if latest.next().is_some() {
        return Err(CatalogError::Corrupt(
            "multiple valid catalog heads at latest generation",
        ));
    }
    Ok(Some(result))
}

fn write_repack_intent(
    root: &Path,
    old: CatalogHead,
    new: (u64, [u8; 32]),
    durability: Durability,
) -> Result<(), CatalogError> {
    let encoded = encode_repack_intent(&RepackIntent {
        old: old.map(catalog_ref),
        new: catalog_ref(new),
    })?;
    atomic_write(root, &root.join(REPACK_INTENT_FILE), &encoded, durability)
}

fn recover_repack_intent(
    root: &Path,
    current: CatalogHead,
    durability: Durability,
) -> Result<(), CatalogError> {
    let path = root.join(REPACK_INTENT_FILE);
    let encoded = match fs::read(&path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let intent = decode_repack_intent(&encoded)?;
    let old = intent.old.map(catalog_head_pair);
    let new = catalog_head_pair(intent.new);
    if current != old && current != Some(new) {
        return Err(CatalogError::Corrupt("repack intent authority mismatch"));
    }
    if current == old {
        let catalogs = root.join("catalog");
        let encoded = read_catalog_bytes(&catalogs, new.0, new.1)?;
        let snapshot = decode_physical_catalog_snapshot(&encoded)?;
        if snapshot.generation != new.0 {
            return Err(CatalogError::Corrupt("repack intent new generation"));
        }
        let segments = root.join("segments");
        for source in authority_sources(&snapshot) {
            let segment_path = root.join(source.path);
            let reader = SegmentReader::open(File::open(&segment_path)?)?;
            if reader.id().0 != source.segment
                || reader.metadata().total_length != source.stored_length
            {
                return Err(CatalogError::Corrupt("repack intent replacement segment"));
            }
            fs::remove_file(segment_path)?;
        }
        fs::remove_file(catalogs.join(format!("{}-{}.catalog", new.0, hex(&new.1))))?;
        durability.sync_directory(&segments)?;
        durability.sync_directory(&catalogs)?;
    }
    // When the new head is authoritative, normal opening authenticates it and
    // then retires the old generation. The intent can be removed now because
    // both branches have reached a self-describing authority.
    remove_durable_file(root, &path, durability)
}

fn atomic_write(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut temporary = NamedTempFile::new_in(root)?;
    temporary.write_all(bytes)?;
    durability.sync_file(temporary.as_file())?;
    temporary.persist(path).map_err(|error| error.error)?;
    durability.sync_directory(root)?;
    Ok(())
}

fn remove_durable_file(
    directory: &Path,
    path: &Path,
    durability: Durability,
) -> Result<(), CatalogError> {
    match fs::remove_file(path) {
        Ok(()) => durability.sync_directory(directory)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn sync_created_ancestors(root: &Path, durability: Durability) -> Result<(), CatalogError> {
    let mut ancestor = Some(root);
    while let Some(path) = ancestor {
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        durability.sync_directory(path)?;
        ancestor = if path == Path::new(".") {
            None
        } else {
            path.parent()
        };
    }
    Ok(())
}

fn i64_value(value: u64) -> Result<i64, CatalogError> {
    i64::try_from(value).map_err(|_| CatalogError::Corrupt("integer overflow"))
}

fn u64_value(value: i64) -> Result<u64, CatalogError> {
    u64::try_from(value).map_err(|_| CatalogError::Corrupt("negative locator integer"))
}

fn digest32(value: Vec<u8>) -> Result<[u8; 32], CatalogError> {
    value
        .try_into()
        .map_err(|_| CatalogError::Corrupt("locator digest"))
}

fn try_lock_exclusive(file: &File) -> Result<(), CatalogError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(CatalogError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(CatalogError::Io(error)),
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

#[cfg(test)]
mod delta_layout_tests {
    use super::*;
    use crate::packed::segment::CONTENT_NAMESPACE;

    fn entry(namespace: u8, digest: u8) -> RepackInventoryEntry {
        RepackInventoryEntry {
            source: RecordSource::Loose,
            namespace,
            kind: 0,
            digest: [digest; 32],
            logical_length: 1,
        }
    }

    /// Sorted inventory of metadata records with digests `[d; 32]`.
    fn metadata(digests: &[u8]) -> Vec<RepackInventoryEntry> {
        digests
            .iter()
            .map(|digest| entry(METADATA_NAMESPACE, *digest))
            .collect()
    }

    fn bases(pairs: &[(u8, u8)]) -> BTreeMap<[u8; 32], [u8; 32]> {
        pairs
            .iter()
            .map(|(target, base)| ([*target; 32], [*base; 32]))
            .collect()
    }

    fn layout(
        inventory: &[RepackInventoryEntry],
        pairs: &[(u8, u8)],
    ) -> (Vec<usize>, Vec<Option<usize>>) {
        delta_layout(inventory, &bases(pairs)).unwrap()
    }

    #[test]
    fn chains_are_written_base_first_and_contiguously() {
        // Digest 1 deltas against 3, which deltas against 4; 2 stands alone.
        let (order, planned) = layout(&metadata(&[1, 2, 3, 4]), &[(1, 3), (3, 4)]);
        assert_eq!(planned, [Some(2), None, Some(3), None]);
        assert_eq!(order, [1, 3, 2, 0]);
    }

    #[test]
    fn a_base_with_several_dependents_finishes_each_subtree_in_key_order() {
        // 5 is the base of 1 and 2; 3 depends on 1.
        let (order, _) = layout(&metadata(&[1, 2, 3, 5]), &[(1, 5), (2, 5), (3, 1)]);
        assert_eq!(order, [3, 0, 2, 1]);
    }

    #[test]
    fn missing_self_and_content_bases_are_dropped() {
        // Inventory sorts by (namespace, digest); content follows metadata.
        let mut inventory = metadata(&[1, 2]);
        inventory.push(entry(CONTENT_NAMESPACE, 7));
        // 1 names a base outside the inventory, 2 names itself, and the
        // content record is never a delta target.
        let (order, planned) = layout(&inventory, &[(1, 9), (2, 2), (7, 1)]);
        assert_eq!(planned, [None, None, None]);
        assert_eq!(order, [0, 1, 2]);
    }

    #[test]
    fn cycles_are_laid_out_completely_from_their_smallest_member() {
        let (order, planned) = layout(&metadata(&[1, 2, 3]), &[(1, 2), (2, 1), (3, 3)]);
        assert_eq!(planned, [Some(1), Some(0), None]);
        // 3 is a real root; the cycle starts at its smallest key and the
        // writer stores that first member whole (its base is not yet written).
        assert_eq!(order, [2, 0, 1]);
    }

    #[test]
    fn layout_is_deterministic_for_equal_inputs() {
        let inventory = metadata(&(1..=40).collect::<Vec<_>>());
        let pairs = (1..40)
            .map(|digest| (digest, digest + 1))
            .collect::<Vec<_>>();
        let first = layout(&inventory, &pairs);
        assert_eq!(first, layout(&inventory, &pairs));
        // A single 40-long chain is laid out tip first, oldest last.
        assert_eq!(first.0, (0..40).rev().collect::<Vec<_>>());
    }
}
