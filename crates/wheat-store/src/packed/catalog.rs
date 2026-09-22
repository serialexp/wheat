//! Durable publication catalog for immutable and active packed segments.
//!
//! `catalog.head` and `active.head` are authority. SQLite is disposable derived
//! state and is rebuilt from the authenticated immutable chain plus the active
//! segment whenever its combined checkpoint disagrees.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    SegmentCatalogRecord, SegmentId, SegmentIndexEntry, SegmentRecordError,
    decode_segment_catalog_record, encode_segment_catalog_record,
};

use super::{
    active::{ActiveError, ActivePublishedState, ActiveRecoveryJournal, ActiveSegment},
    bulk::{BulkSegment, FinalizedBulk},
    segment::{
        MAX_SEGMENT_PAYLOAD_BYTES, MAX_SEGMENT_TAIL_BYTES, PreparedSegmentRecord, SealedSegment,
        SegmentBuilder, SegmentError, SegmentReader, SegmentRecord, prepare_record, verify_logical,
    },
    trace::PackedStoreTraceCounters,
};
use crate::Durability;

const SCHEMA: &str = include_str!("catalog_sql/schema.sql");
const LOOKUP: &str = include_str!("catalog_sql/lookup.sql");
const SCHEMA_VERSION: i64 = 5;
const SEGMENT_SUFFIX: &str = ".segment";
const CATALOG_SUFFIX: &str = ".catalog";
const CATALOG_HEAD_FILE: &str = "catalog.head";
const ACTIVE_FILE: &str = "active.segment";
const ACTIVE_HEAD_FILE: &str = "active.head";
const ACTIVE_JOURNAL_FILE: &str = "active.append";
const BULK_FILE_PREFIX: &str = "bulk-";
const CATALOG_HEAD_MAGIC: &[u8; 9] = b"WHEATPACK";
const CATALOG_HEAD_VERSION: u8 = 1;
const CATALOG_HEAD_LEN: usize = CATALOG_HEAD_MAGIC.len() + 1 + 8 + 32;
const ACTIVE_HEAD_MAGIC: &[u8; 8] = b"WHEATAHD";
const ACTIVE_JOURNAL_MAGIC: &[u8; 8] = b"WHEATAJR";
const ACTIVE_FORMAT_VERSION: u8 = 1;
const STATE_ENCODED_LEN: usize = 8 + 32 + 6 * 8;
const CHECKSUM_LEN: usize = 32;
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
}

struct CatalogItem {
    record: SegmentCatalogRecord,
    digest: [u8; 32],
}

#[derive(Clone)]
struct SealedSource {
    path: String,
    segment: [u8; 32],
    stored_length: u64,
}

struct DeferredBulk {
    source: String,
    segment: BulkSegment,
}

type CatalogHead = Option<(u64, [u8; 32])>;
type CatalogCheckpoint = (CatalogHead, ActivePublishedState, u64);
type LocatorRow = (u16, u64, u8, u64, u64, String);

/// A process-exclusive writer/read facade over a packed-store root.
pub(crate) struct PackedCatalog {
    root: PathBuf,
    segments: PathBuf,
    catalogs: PathBuf,
    _lock: File,
    index: Connection,
    generation: Option<u64>,
    head: Option<[u8; 32]>,
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
    deferred_failpoint: Option<DeferredFailpoint>,
    #[cfg(test)]
    close_failpoint: Option<CloseFailpoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppendFailpoint {
    MutationApplied,
    HeadPublished,
    JournalRemoved,
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
                let head = latest_catalog_head(&catalogs)?;
                write_catalog_head(&root, &catalog_head_path, head, durability)?;
                head
            }
            Err(error) => return Err(error),
        };
        validate_catalog_head(&catalogs, durable_head)?;
        let catalog_chain = read_chain(&catalogs, durable_head)?;
        let committed_segment = catalog_chain.last().map(|item| item.record.segment);
        let (active, active_retired) = recover_active(&root, durability, committed_segment)?;
        remove_unpublished_bulk_files(&segments, durability)?;
        remove_unreachable_publications(&segments, &catalogs, &catalog_chain, durability)?;

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
        let sealed_sources = catalog_chain
            .into_iter()
            .map(|item| SealedSource {
                path: format!(
                    "segments/{}-{}.segment",
                    item.record.generation,
                    hex(&item.record.segment)
                ),
                segment: item.record.segment,
                stored_length: item.record.stored_length,
            })
            .collect();
        let mut catalog = Self {
            root,
            segments,
            catalogs,
            _lock: lock,
            index,
            generation: durable_head.map(|(generation, _)| generation),
            head: durable_head.map(|(_, digest)| digest),
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
            deferred_failpoint: None,
            #[cfg(test)]
            close_failpoint: None,
        };
        let authority = (durable_head, catalog.active.state());
        let checkpoint = catalog.read_checkpoint();
        let checkpoint_valid = checkpoint
            .as_ref()
            .is_ok_and(|(head, active, _)| (*head, *active) == authority);
        if active_retired || !checkpoint_valid {
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
    pub(crate) fn test_interrupt_append(
        &mut self,
        records: &[SegmentRecord<'_>],
        publish_intended_head: bool,
    ) -> Result<(), CatalogError> {
        let intended = self.active.state().generation + 1;
        let prepared = self.active.prepare_append(records, intended)?;
        let journal = prepared.journal().clone();
        write_active_journal(&self.root, &journal, self.durability)?;
        let result = self.active.commit_append(prepared)?;
        self.active.sync(self.durability)?;
        if publish_intended_head {
            write_active_head(&self.root, result.state, self.durability)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_set_seal_failpoint(&mut self, failpoint: SealFailpoint) {
        self.seal_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn test_set_append_failpoint(&mut self, failpoint: AppendFailpoint) {
        self.append_failpoint = Some(failpoint);
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
        records: &[SegmentRecord<'_>],
    ) -> Result<(), CatalogError> {
        if self.durability == Durability::Deferred {
            return self.put_records_deferred(records);
        }
        self.put_records_durable(records)
    }

    fn put_records_durable(&mut self, records: &[SegmentRecord<'_>]) -> Result<(), CatalogError> {
        if self.poisoned {
            return Err(CatalogError::Poisoned);
        }
        if records.is_empty() {
            return Ok(());
        }
        // Resolve and deduplicate keys before projecting the active size. A
        // duplicate-heavy batch must not cause a false pre-seal.
        let mut pending = Vec::with_capacity(records.len());
        let mut pending_by_key = HashMap::with_capacity(records.len());
        for record in records {
            super::segment::validate_record(*record)?;
            if let Some(&index) = pending_by_key.get(&(record.namespace, record.digest)) {
                let previous: SegmentRecord<'_> = pending[index];
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
                pending.push(*record);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        // Prepare every new logical record exactly once. Partitioning and all
        // downstream writers operate on these stored bytes, while duplicate and
        // existing-key checks above deliberately compare the logical bytes.
        let mut prepared = pending
            .into_iter()
            .map(prepare_record)
            .collect::<Result<Vec<_>, _>>()?;
        // Reject any individually impossible record before sealing or appending
        // earlier records from this call. The target is advisory for one record,
        // but the format's count and stored-payload bounds are not.
        for record in &prepared {
            self.projected_physical_size(0, 0, 1, record.stored_length())?;
        }

        // Partition at the catalog boundary. Adapters may submit arbitrarily
        // large batches; each active append remains bounded by the format and
        // target, while a record which cannot fit an empty segment still fails.
        let mut start = 0;
        while start < prepared.len() {
            let mut end = start;
            let mut payload = 0_u64;
            while end < prepared.len() {
                let Some(next_payload) = payload.checked_add(prepared[end].stored_length()) else {
                    break;
                };
                let projected = self.projected_physical_size(
                    self.active.entries().len(),
                    self.active.state().metadata.payload_bytes,
                    end - start + 1,
                    next_payload,
                );
                let fits = projected
                    .as_ref()
                    .is_ok_and(|&size| size <= self.active_target_bytes);
                if !fits && end != start {
                    break;
                }
                if !fits && self.active.state().metadata.object_count != 0 {
                    self.seal_active()?;
                    payload = 0;
                    end = start;
                    continue;
                }
                payload = next_payload;
                end += 1;
                if projected.is_ok_and(|size| size > self.active_target_bytes) {
                    break;
                }
            }
            if end == start {
                return Err(SegmentError::TooLarge.into());
            }
            self.append_active_batch(prepared.drain(start..end).collect())?;
            start = 0;
            if !prepared.is_empty()
                || self.active.state().metadata.total_length >= self.active_target_bytes
                || self.active.should_seal()
            {
                self.seal_active()?;
            }
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
        Ok(super::segment::projected_total_length(count, payload)?)
    }

    fn append_active_batch(
        &mut self,
        records: Vec<PreparedSegmentRecord<'_>>,
    ) -> Result<(), CatalogError> {
        let intended_generation = self
            .active
            .state()
            .generation
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("active generation overflow"))?;
        let prepared = self
            .active
            .prepare_prepared_append(records, intended_generation)?;
        let journal = prepared.journal().clone();
        // Every operation from journal publication onward can leave observable
        // state behind even when it returns an error. Keep this instance closed
        // to writes until the entire authority + derived-index protocol completes.
        self.poisoned = true;
        write_active_journal(&self.root, &journal, self.durability)?;
        let result = self.active.commit_append(prepared)?;
        #[cfg(test)]
        if self.append_failpoint == Some(AppendFailpoint::MutationApplied) {
            return Err(CatalogError::Corrupt("test failure after active mutation"));
        }
        self.active.sync(self.durability)?;
        write_active_head(&self.root, result.state, self.durability)?;
        #[cfg(test)]
        if self.append_failpoint == Some(AppendFailpoint::HeadPublished) {
            return Err(CatalogError::Corrupt("test failure after active head"));
        }
        remove_durable_file(
            &self.root,
            &self.root.join(ACTIVE_JOURNAL_FILE),
            self.durability,
        )?;
        #[cfg(test)]
        if self.append_failpoint == Some(AppendFailpoint::JournalRemoved) {
            return Err(CatalogError::Corrupt("test failure after journal removal"));
        }
        self.index_active_entries_and_checkpoint(&result.added_entries)?;
        self.poisoned = false;
        Ok(())
    }

    fn put_records_deferred(&mut self, records: &[SegmentRecord<'_>]) -> Result<(), CatalogError> {
        if self.poisoned {
            return Err(CatalogError::Poisoned);
        }
        if let Some(trace) = &self.trace {
            trace.deferred_put(records.len());
        }
        for record in records {
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
            let prepared = prepare_record(*record)?;
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
        let segment_name = format!("{}-{}{}", generation, hex(&finalized.id.0), SEGMENT_SUFFIX);
        let segment_path = self.segments.join(&segment_name);
        fs::rename(&finalized.path, &segment_path)?;
        let record = SegmentCatalogRecord {
            generation,
            previous: self.head,
            segment: finalized.id.0,
            stored_length: finalized.metadata.total_length,
        };
        let digest = publish_catalog_record(&self.root, &self.catalogs, &record, self.durability)?;
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
            segment: finalized.id.0,
            stored_length: finalized.metadata.total_length,
        });
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
        let started = Instant::now();
        if let Some(trace) = &self.trace {
            trace.catalog_get();
        }
        let result = self.get_bounded_inner(namespace, digest, max);
        if let Some(trace) = &self.trace {
            trace.catalog_lookup_duration(started.elapsed());
        }
        result
    }

    fn get_bounded_inner(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        if let Some(value) = self
            .bulk
            .as_mut()
            .map(|bulk| bulk.segment.lookup_bounded(namespace, digest, max))
            .transpose()?
            .flatten()
        {
            if let Some(trace) = &self.trace {
                trace.catalog_bulk_hit();
            }
            return Ok(Some(value));
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
                        .lookup_authority_bounded(namespace, digest, None, max)?
                        .is_some()
                {
                    if attempt == 0 {
                        self.rebuild_index()?;
                        continue;
                    }
                    return Err(CatalogError::Corrupt("locator misses authoritative object"));
                }
                match self.read_authoritative_location(namespace, digest, location.as_ref(), max) {
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

    fn read_authoritative_location(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        locator: Option<&LocatorRow>,
        max: usize,
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
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
        if *logical_length > max as u64 {
            return Err(CatalogError::Segment(SegmentError::ReadLimit {
                minimum: usize::try_from(*logical_length).unwrap_or(usize::MAX),
            }));
        }
        let Some((entry, bytes)) =
            self.lookup_authority_bounded(namespace, digest, Some(source), max)?
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
        verify_logical(&entry, &bytes)?;
        Ok(Some((*kind, bytes)))
    }

    fn lookup_authority_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        requested_source: Option<&str>,
        max: usize,
    ) -> Result<Option<(SegmentIndexEntry, Vec<u8>)>, CatalogError> {
        if requested_source.is_none_or(|source| source == ACTIVE_FILE) {
            if let Some(value) = self.active.lookup_entry_bounded(namespace, digest, max)? {
                return Ok(Some(value));
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
            if let Some((entry, bytes)) = reader.lookup_entry_bounded(namespace, digest, max)? {
                return Ok(Some((entry.clone(), bytes)));
            }
        }
        Ok(None)
    }

    fn seal_active(&mut self) -> Result<(), CatalogError> {
        if self.active.state().metadata.object_count == 0 {
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

        let segment_name = format!("{}-{}{}", generation, hex(&sealed.id.0), SEGMENT_SUFFIX);
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

        let record = SegmentCatalogRecord {
            generation,
            previous: self.head,
            segment: sealed.id.0,
            stored_length: sealed.metadata.total_length,
        };
        let digest = publish_catalog_record(&self.root, &self.catalogs, &record, self.durability)?;
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
            &record,
            &mut self.active,
            self.durability,
            fail_during_retirement,
        )?;
        self.index_active_and_checkpoint()?;
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
            builder.push_prepared(prepare_record(record)?)?;
        }
        Ok(builder.seal()?)
    }

    fn rebuild_index(&mut self) -> Result<(), CatalogError> {
        self.load_sealed_readers()?;
        let catalog_head = self.generation.zip(self.head);
        let items = read_chain(&self.catalogs, catalog_head)?;
        let transaction = self.index.transaction()?;
        transaction.execute("DELETE FROM locator", [])?;
        transaction.execute("DELETE FROM physical_source", [])?;
        for item in &items {
            let filename = format!(
                "{}-{}.segment",
                item.record.generation,
                hex(&item.record.segment)
            );
            let relative = format!("segments/{filename}");
            let reader =
                self.sealed_readers
                    .get(&item.record.segment)
                    .ok_or(CatalogError::Corrupt(
                        "sealed descriptor lacks cached reader",
                    ))?;
            insert_source_entries(&transaction, &relative, reader.authenticated_entries())?;
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

    fn index_active_entries_and_checkpoint(
        &mut self,
        entries: &[SegmentIndexEntry],
    ) -> Result<(), CatalogError> {
        let added = u64::try_from(entries.len())
            .map_err(|_| CatalogError::Corrupt("locator count overflow"))?;
        let locator_rows = self
            .locator_rows
            .checked_add(added)
            .ok_or(CatalogError::Corrupt("locator count overflow"))?;
        let transaction = self.index.transaction()?;
        insert_source_entries(&transaction, ACTIVE_FILE, entries)?;
        write_checkpoint(
            &transaction,
            self.generation.zip(self.head),
            self.active.state(),
            locator_rows,
        )?;
        transaction.commit()?;
        self.locator_rows = locator_rows;
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
            generation: _,
            head: _,
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
                deferred_failpoint: _,
            #[cfg(test)]
                close_failpoint: _,
        } = self;

        let close_result = index.close();
        drop(active);
        drop(_lock);
        drop(catalogs);
        drop(segments);
        drop(root);

        match close_result {
            Ok(()) => Ok(()),
            Err((connection, error)) => {
                drop(connection);
                Err(CatalogError::Database(error))
            }
        }
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
             active_object_count,active_payload_bytes,active_total_length,active_index_start,\
             active_directory_offset,active_directory_length,locator_count \
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
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
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
            metadata: super::segment::SegmentMetadata {
                object_count: u64_value(row.4)?,
                payload_bytes: u64_value(row.5)?,
                total_length: u64_value(row.6)?,
                index_start: u64_value(row.7)?,
                directory_offset: u64_value(row.8)?,
                directory_length: u64_value(row.9)?,
            },
        };
        Ok((catalog, active, u64_value(row.10)?))
    }
}

fn recover_active(
    root: &Path,
    durability: Durability,
    committed_segment: Option<[u8; 32]>,
) -> Result<(ActiveSegment, bool), CatalogError> {
    let active_path = root.join(ACTIVE_FILE);
    let head_path = root.join(ACTIVE_HEAD_FILE);
    let journal_path = root.join(ACTIVE_JOURNAL_FILE);
    if !head_path.exists() {
        if journal_path.exists() {
            return Err(CatalogError::Corrupt(
                "active journal exists without active head",
            ));
        }
        let active = if active_path.exists() {
            // Creation publishes the complete empty V1 segment before its head.
            // Recover that one safe interruption window, but reject arbitrary or
            // non-empty legacy files lacking publication metadata.
            let reader = SegmentReader::open(File::open(&active_path)?)?;
            if reader.metadata().object_count != 0 {
                return Err(CatalogError::Corrupt(
                    "non-empty active segment exists without active head",
                ));
            }
            ActiveSegment::reopen(
                &active_path,
                ActivePublishedState {
                    generation: 0,
                    id: reader.id(),
                    metadata: reader.metadata(),
                },
            )?
        } else {
            ActiveSegment::create(&active_path, 0)?
        };
        active.sync(durability)?;
        durability.sync_directory(root)?;
        write_active_head(root, active.state(), durability)?;
        return Ok((active, false));
    }
    let published = read_active_head(&head_path)?;
    if committed_segment == Some(published.id.0) {
        let next_generation = published
            .generation
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("active generation overflow"))?;
        remove_durable_file(root, &active_path, durability)?;
        remove_durable_file(root, &journal_path, durability)?;
        let active = ActiveSegment::create(&active_path, next_generation)?;
        active.sync(durability)?;
        durability.sync_directory(root)?;
        write_active_head(root, active.state(), durability)?;
        return Ok((active, true));
    }
    let Some(journal) = read_optional_active_journal(&journal_path)? else {
        return Ok((ActiveSegment::reopen(active_path, published)?, false));
    };
    if published == journal.old {
        let active = ActiveSegment::recover(&active_path, &journal)?;
        active.sync(durability)?;
        remove_durable_file(root, &journal_path, durability)?;
        Ok((active, false))
    } else if published.generation == journal.intended_generation {
        let active = ActiveSegment::reopen(&active_path, published)?;
        remove_durable_file(root, &journal_path, durability)?;
        Ok((active, false))
    } else {
        Err(CatalogError::Corrupt("active journal does not match head"))
    }
}

fn retire_active(
    root: &Path,
    record: &SegmentCatalogRecord,
    active: &mut ActiveSegment,
    durability: Durability,
    fail_before_rename: bool,
) -> Result<(), CatalogError> {
    if active.state().id.0 != record.segment {
        return Err(CatalogError::Corrupt("retired active identity"));
    }
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
    let replacement = ActiveSegment::create(&temporary_path, next_generation)?;
    replacement.sync(durability)?;
    if fail_before_rename {
        return Err(CatalogError::Corrupt(
            "test failure during active retirement",
        ));
    }
    fs::rename(&temporary_path, root.join(ACTIVE_FILE))?;
    durability.sync_directory(root)?;
    write_active_head(root, replacement.state(), durability)?;
    *active = replacement;
    Ok(())
}

fn open_index(path: &Path, durability: Durability) -> Result<Connection, CatalogError> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.pragma_update(
        None,
        "synchronous",
        match durability {
            Durability::Durable => "FULL",
            Durability::Deferred => "OFF",
        },
    )?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -8192)?;
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
/// absent from the chain rooted at the already-validated authoritative head.
/// Unknown, non-canonical, and malformed entries are deliberately preserved.
fn remove_unreachable_publications(
    segments: &Path,
    catalogs: &Path,
    chain: &[CatalogItem],
    durability: Durability,
) -> Result<(), CatalogError> {
    let reachable_catalogs: HashSet<String> = chain
        .iter()
        .map(|item| {
            format!(
                "{}-{}{}",
                item.record.generation,
                hex(&item.digest),
                CATALOG_SUFFIX
            )
        })
        .collect();
    let reachable_segments: HashSet<String> = chain
        .iter()
        .map(|item| {
            format!(
                "{}-{}{}",
                item.record.generation,
                hex(&item.record.segment),
                SEGMENT_SUFFIX
            )
        })
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
        let Ok(record) = decode_segment_catalog_record(&encoded) else {
            continue;
        };
        if record.generation != generation {
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
        let Some((_generation, segment_id)) =
            parse_canonical_publication_name(&name, SEGMENT_SUFFIX)
        else {
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

fn parse_canonical_publication_name(name: &str, suffix: &str) -> Option<(u64, [u8; 32])> {
    let stem = name.strip_suffix(suffix)?;
    let (generation_text, digest_text) = stem.split_once('-')?;
    if generation_text.is_empty()
        || (generation_text.len() > 1 && generation_text.starts_with('0'))
        || digest_text.len() != 64
        || !digest_text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let generation = generation_text.parse().ok()?;
    let mut digest = [0; 32];
    for (index, pair) in digest_text.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => unreachable!(),
        };
        digest[index] = nibble(pair[0]) << 4 | nibble(pair[1]);
    }
    Some((generation, digest))
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
         active_index_start=?8,active_directory_offset=?9,active_directory_length=?10,\
         locator_count=?11 WHERE singleton=1",
        params![
            generation,
            digest.as_ref().map(|value| value.as_slice()),
            i64_value(active.generation)?,
            active.id.0.as_slice(),
            i64_value(active.metadata.object_count)?,
            i64_value(active.metadata.payload_bytes)?,
            i64_value(active.metadata.total_length)?,
            i64_value(active.metadata.index_start)?,
            i64_value(active.metadata.directory_offset)?,
            i64_value(active.metadata.directory_length)?,
            locator_count,
        ],
    )?;
    Ok(())
}

fn publish_catalog_record(
    root: &Path,
    catalogs: &Path,
    record: &SegmentCatalogRecord,
    durability: Durability,
) -> Result<[u8; 32], CatalogError> {
    let encoded = encode_segment_catalog_record(record)?;
    let digest = *blake3::hash(&encoded).as_bytes();
    let path = catalogs.join(format!("{}-{}.catalog", record.generation, hex(&digest)));
    let mut temporary = NamedTempFile::new_in(root)?;
    temporary.write_all(&encoded)?;
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

fn read_catalog_head(path: &Path) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
    let encoded = fs::read(path)?;
    if encoded.len() == CATALOG_HEAD_MAGIC.len() + 1 && encoded.starts_with(CATALOG_HEAD_MAGIC) {
        return (encoded[CATALOG_HEAD_MAGIC.len()] == CATALOG_HEAD_VERSION)
            .then_some(None)
            .ok_or(CatalogError::Corrupt("catalog head version"));
    }
    if encoded.len() != CATALOG_HEAD_LEN
        || !encoded.starts_with(CATALOG_HEAD_MAGIC)
        || encoded[CATALOG_HEAD_MAGIC.len()] != CATALOG_HEAD_VERSION
    {
        return Err(CatalogError::Corrupt("catalog head format"));
    }
    let generation = u64::from_be_bytes(encoded[10..18].try_into().unwrap());
    Ok(Some((generation, encoded[18..50].try_into().unwrap())))
}

fn write_catalog_head(
    root: &Path,
    path: &Path,
    head: Option<(u64, [u8; 32])>,
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut encoded = Vec::with_capacity(CATALOG_HEAD_LEN);
    encoded.extend_from_slice(CATALOG_HEAD_MAGIC);
    encoded.push(CATALOG_HEAD_VERSION);
    if let Some((generation, digest)) = head {
        encoded.extend_from_slice(&generation.to_be_bytes());
        encoded.extend_from_slice(&digest);
    }
    atomic_write(root, path, &encoded, durability)
}

fn validate_catalog_head(
    directory: &Path,
    head: Option<(u64, [u8; 32])>,
) -> Result<(), CatalogError> {
    if let Some((generation, digest)) = head {
        read_catalog_record(directory, generation, digest)?;
    }
    Ok(())
}

fn read_catalog_record(
    directory: &Path,
    generation: u64,
    digest: [u8; 32],
) -> Result<SegmentCatalogRecord, CatalogError> {
    let path = directory.join(format!("{}-{}{}", generation, hex(&digest), CATALOG_SUFFIX));
    let encoded = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CatalogError::Corrupt("catalog head references missing record")
        } else {
            CatalogError::Io(error)
        }
    })?;
    let record = decode_segment_catalog_record(&encoded)?;
    if record.generation != generation || blake3::hash(&encoded).as_bytes() != &digest {
        return Err(CatalogError::Corrupt("catalog record identity"));
    }
    Ok(record)
}

fn latest_catalog_head(directory: &Path) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(CATALOG_SUFFIX) else {
            continue;
        };
        let Some((generation_text, digest_text)) = stem.split_once('-') else {
            continue;
        };
        if generation_text.is_empty()
            || (generation_text.len() > 1 && generation_text.starts_with('0'))
            || digest_text.len() != 64
            || !digest_text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        let Ok(generation) = generation_text.parse::<u64>() else {
            continue;
        };
        let mut digest = [0; 32];
        for (index, pair) in digest_text.as_bytes().chunks_exact(2).enumerate() {
            let nibble = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => unreachable!(),
            };
            digest[index] = nibble(pair[0]) << 4 | nibble(pair[1]);
        }
        let encoded = fs::read(entry.path())?;
        if blake3::hash(&encoded).as_bytes() != &digest {
            continue;
        }
        let Ok(record) = decode_segment_catalog_record(&encoded) else {
            continue;
        };
        if record.generation != generation {
            continue;
        }
        let head = (generation, digest);
        if read_chain(directory, Some(head)).is_ok() {
            candidates.push(head);
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

fn read_chain(
    directory: &Path,
    mut head: Option<(u64, [u8; 32])>,
) -> Result<Vec<CatalogItem>, CatalogError> {
    let mut reversed = Vec::new();
    while let Some((generation, digest)) = head {
        let record = read_catalog_record(directory, generation, digest)?;
        let previous = record.previous;
        reversed.push(CatalogItem { record, digest });
        head = match (generation, previous) {
            (0, None) => None,
            (0, Some(_)) => return Err(CatalogError::Corrupt("catalog generation underflow")),
            (_, Some(previous)) => Some((generation - 1, previous)),
            (_, None) => return Err(CatalogError::Corrupt("broken previous-record hash")),
        };
    }
    reversed.reverse();
    Ok(reversed)
}

fn encode_state(state: ActivePublishedState, output: &mut Vec<u8>) {
    output.extend_from_slice(&state.generation.to_be_bytes());
    output.extend_from_slice(&state.id.0);
    for value in [
        state.metadata.object_count,
        state.metadata.payload_bytes,
        state.metadata.total_length,
        state.metadata.index_start,
        state.metadata.directory_offset,
        state.metadata.directory_length,
    ] {
        output.extend_from_slice(&value.to_be_bytes());
    }
}

fn decode_state(bytes: &[u8]) -> Result<ActivePublishedState, CatalogError> {
    if bytes.len() != STATE_ENCODED_LEN {
        return Err(CatalogError::Corrupt("active state length"));
    }
    let value = |start| u64::from_be_bytes(bytes[start..start + 8].try_into().unwrap());
    Ok(ActivePublishedState {
        generation: value(0),
        id: SegmentId(bytes[8..40].try_into().unwrap()),
        metadata: super::segment::SegmentMetadata {
            object_count: value(40),
            payload_bytes: value(48),
            total_length: value(56),
            index_start: value(64),
            directory_offset: value(72),
            directory_length: value(80),
        },
    })
}

fn checked_envelope<'a>(encoded: &'a [u8], magic: &[u8; 8]) -> Result<&'a [u8], CatalogError> {
    if encoded.len() < magic.len() + 1 + CHECKSUM_LEN
        || !encoded.starts_with(magic)
        || encoded[magic.len()] != ACTIVE_FORMAT_VERSION
    {
        return Err(CatalogError::Corrupt("active control format"));
    }
    let body_end = encoded.len() - CHECKSUM_LEN;
    if blake3::hash(&encoded[..body_end]).as_bytes() != &encoded[body_end..] {
        return Err(CatalogError::Corrupt("active control checksum"));
    }
    Ok(&encoded[magic.len() + 1..body_end])
}

fn write_active_head(
    root: &Path,
    state: ActivePublishedState,
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut encoded = Vec::with_capacity(8 + 1 + STATE_ENCODED_LEN + CHECKSUM_LEN);
    encoded.extend_from_slice(ACTIVE_HEAD_MAGIC);
    encoded.push(ACTIVE_FORMAT_VERSION);
    encode_state(state, &mut encoded);
    encoded.extend_from_slice(blake3::hash(&encoded).as_bytes());
    atomic_write(root, &root.join(ACTIVE_HEAD_FILE), &encoded, durability)
}

fn read_active_head(path: &Path) -> Result<ActivePublishedState, CatalogError> {
    let encoded = fs::read(path)?;
    decode_state(checked_envelope(&encoded, ACTIVE_HEAD_MAGIC)?)
}

fn write_active_journal(
    root: &Path,
    journal: &ActiveRecoveryJournal,
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut encoded = Vec::with_capacity(
        8 + 1 + STATE_ENCODED_LEN + 8 + 8 + journal.old_tail.len() + CHECKSUM_LEN,
    );
    encoded.extend_from_slice(ACTIVE_JOURNAL_MAGIC);
    encoded.push(ACTIVE_FORMAT_VERSION);
    encode_state(journal.old, &mut encoded);
    encoded.extend_from_slice(&journal.intended_generation.to_be_bytes());
    encoded.extend_from_slice(&(journal.old_tail.len() as u64).to_be_bytes());
    encoded.extend_from_slice(&journal.old_tail);
    encoded.extend_from_slice(blake3::hash(&encoded).as_bytes());
    atomic_write(root, &root.join(ACTIVE_JOURNAL_FILE), &encoded, durability)
}

fn read_optional_active_journal(
    path: &Path,
) -> Result<Option<ActiveRecoveryJournal>, CatalogError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let maximum = (ACTIVE_JOURNAL_MAGIC.len() + 1 + STATE_ENCODED_LEN + 16 + CHECKSUM_LEN) as u64
        + MAX_SEGMENT_TAIL_BYTES;
    let file_len = file.metadata()?.len();
    if file_len > maximum {
        return Err(CatalogError::Corrupt("active journal exceeds tail limit"));
    }
    let mut encoded = Vec::with_capacity(file_len as usize);
    file.read_to_end(&mut encoded)?;
    let body = checked_envelope(&encoded, ACTIVE_JOURNAL_MAGIC)?;
    if body.len() < STATE_ENCODED_LEN + 16 {
        return Err(CatalogError::Corrupt("active journal length"));
    }
    let old = decode_state(&body[..STATE_ENCODED_LEN])?;
    let intended_generation = u64::from_be_bytes(
        body[STATE_ENCODED_LEN..STATE_ENCODED_LEN + 8]
            .try_into()
            .unwrap(),
    );
    let tail_len = u64::from_be_bytes(
        body[STATE_ENCODED_LEN + 8..STATE_ENCODED_LEN + 16]
            .try_into()
            .unwrap(),
    );
    let tail_len = usize::try_from(tail_len).map_err(|_| CatalogError::Corrupt("journal tail"))?;
    if body.len() != STATE_ENCODED_LEN + 16 + tail_len {
        return Err(CatalogError::Corrupt("active journal tail length"));
    }
    Ok(Some(ActiveRecoveryJournal {
        old,
        intended_generation,
        old_tail: body[STATE_ENCODED_LEN + 16..].to_vec(),
    }))
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
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
    {
        Err(CatalogError::Busy)
    } else {
        Err(CatalogError::Io(error))
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
