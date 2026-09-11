//! Process-local mutable V1 segment growth.
//!
//! An active segment always contains a complete, authenticated V1 segment at
//! its published length. Appends retain the prefix and existing payload bytes,
//! replacing only the cumulative index/directory/footer tail.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use thiserror::Error;
use wheat_core::{
    SEGMENT_PREFIX_ENCODED_SIZE, SegmentId, SegmentIndexEntry, encode_segment_prefix,
};

use super::segment::{
    MAX_SEGMENT_ENTRIES, MAX_SEGMENT_PAYLOAD_BYTES, MAX_SEGMENT_TAIL_BYTES, SegmentError,
    SegmentMetadata, SegmentReader, SegmentRecord, emit_cumulative_tail,
};
use crate::Durability;

const PREFIX_LEN: u64 = SEGMENT_PREFIX_ENCODED_SIZE as u64;
const IO_BUFFER_SIZE: usize = 64 * 1024;
pub(crate) const ACTIVE_SEGMENT_TARGET_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActivePublishedState {
    pub generation: u64,
    pub id: SegmentId,
    pub metadata: SegmentMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveRecoveryJournal {
    pub old: ActivePublishedState,
    pub intended_generation: u64,
    /// Exact old index, directory, and footer bytes beginning at `index_start`.
    pub old_tail: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveAppendResult {
    pub state: ActivePublishedState,
    pub added_entries: Vec<SegmentIndexEntry>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActiveSealMetadata {
    pub id: SegmentId,
    pub stored_length: u64,
    pub object_count: u64,
}

#[derive(Debug, Error)]
pub(crate) enum ActiveError {
    #[error("active segment failed: {0}")]
    Segment(#[from] SegmentError),
    #[error("active segment I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("published active-segment state does not match the file")]
    PublishedStateMismatch,
    #[error("active-segment generation must increase by exactly one")]
    Generation,
    #[error("existing segment key differs from proposed bytes")]
    ConflictingKey,
    #[error("prepared append no longer applies to this active segment")]
    StaleAppend,
    #[error("active segment is unusable until it is reopened or restored")]
    Poisoned,
    #[error("an append must add at least one new record")]
    NoChanges,
}

pub(crate) struct PreparedAppend<'a> {
    old: ActivePublishedState,
    intended_generation: u64,
    records: Vec<SegmentRecord<'a>>,
    journal: ActiveRecoveryJournal,
}

impl PreparedAppend<'_> {
    /// Bytes which the caller must durably publish before `commit_append`.
    pub(crate) fn journal(&self) -> &ActiveRecoveryJournal {
        &self.journal
    }

    #[cfg(test)]
    pub(crate) fn is_retry(&self) -> bool {
        self.records.is_empty()
    }
}

fn validate_projected(count: usize, payload_bytes: u64) -> Result<(), SegmentError> {
    if count > MAX_SEGMENT_ENTRIES {
        return Err(SegmentError::TooManyEntries);
    }
    if payload_bytes > MAX_SEGMENT_PAYLOAD_BYTES {
        return Err(SegmentError::TooLarge);
    }
    Ok(())
}

pub(crate) struct ActiveSegment {
    file: File,
    state: ActivePublishedState,
    entries: Vec<SegmentIndexEntry>,
    io_buffer: Vec<u8>,
    poisoned: bool,
    #[cfg(test)]
    fail_commit_after_payload: bool,
}

impl ActiveSegment {
    /// Creates a new process-local active segment with a valid empty V1 tail.
    pub(crate) fn create(path: impl AsRef<Path>, generation: u64) -> Result<Self, ActiveError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&encode_segment_prefix().map_err(SegmentError::from)?)?;
        let mut entries = Vec::new();
        let mut io_buffer = vec![0; IO_BUFFER_SIZE];
        let footer = emit_cumulative_tail(&mut file, &mut entries, PREFIX_LEN, &mut io_buffer)?;
        let state = ActivePublishedState {
            generation,
            id: footer.segment_id(),
            metadata: super::segment::metadata(&footer),
        };
        Ok(Self {
            file,
            state,
            entries,
            io_buffer,
            poisoned: false,
            #[cfg(test)]
            fail_commit_after_payload: false,
        })
    }

    /// Reopens only when the complete V1 file authenticates as the supplied
    /// published state. Bytes beyond its published length are ignored first.
    pub(crate) fn reopen(
        path: impl AsRef<Path>,
        published: ActivePublishedState,
    ) -> Result<Self, ActiveError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let reader = SegmentReader::open_at_published_length(
            file.try_clone()?,
            published.metadata.total_length,
        )?;
        if reader.id() != published.id || reader.metadata() != published.metadata {
            return Err(ActiveError::PublishedStateMismatch);
        }
        let (reader_file, _, _, entries) = reader.into_authenticated_parts();
        drop(reader_file);
        Ok(Self {
            file,
            state: published,
            entries,
            io_buffer: vec![0; IO_BUFFER_SIZE],
            poisoned: false,
            #[cfg(test)]
            fail_commit_after_payload: false,
        })
    }

    /// Opens a possibly interrupted file at the journal's old state and restores
    /// its authenticated old tail before any caller can observe it.
    pub(crate) fn recover(
        path: impl AsRef<Path>,
        journal: &ActiveRecoveryJournal,
    ) -> Result<Self, ActiveError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut active = Self {
            file,
            state: journal.old,
            entries: Vec::new(),
            io_buffer: vec![0; IO_BUFFER_SIZE],
            poisoned: true,
            #[cfg(test)]
            fail_commit_after_payload: false,
        };
        active.restore(journal)?;
        Ok(active)
    }

    pub(crate) fn state(&self) -> ActivePublishedState {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn test_fail_commit_after_payload(&mut self) {
        self.fail_commit_after_payload = true;
    }

    /// Validates/deduplicates a bounded batch and captures the exact old tail.
    /// No segment bytes are changed by this method.
    pub(crate) fn prepare_append<'a>(
        &mut self,
        records: &[SegmentRecord<'a>],
        intended_generation: u64,
    ) -> Result<PreparedAppend<'a>, ActiveError> {
        if self.poisoned {
            return Err(ActiveError::Poisoned);
        }
        let retry = intended_generation == self.state.generation;
        if !retry && self.state.generation.checked_add(1) != Some(intended_generation) {
            return Err(ActiveError::Generation);
        }

        let mut accepted = Vec::with_capacity(records.len());
        let mut accepted_by_key = HashMap::with_capacity(records.len());
        let mut projected_payload = self.state.metadata.payload_bytes;
        for record in records {
            super::segment::validate_record(*record)?;
            if let Some((kind, offset, length)) = self.find_entry(record.namespace, record.digest) {
                if kind != record.kind || !self.payload_equals(offset, length, record.bytes)? {
                    return Err(ActiveError::ConflictingKey);
                }
                continue;
            }
            if retry {
                return Err(ActiveError::StaleAppend);
            }
            let record_key = (record.namespace, record.digest);
            if let Some(&index) = accepted_by_key.get(&record_key) {
                let existing: &SegmentRecord<'_> = &accepted[index];
                if existing.kind != record.kind || existing.bytes != record.bytes {
                    return Err(ActiveError::ConflictingKey);
                }
                continue;
            }
            let projected_count = self
                .entries
                .len()
                .checked_add(accepted.len())
                .and_then(|count| count.checked_add(1))
                .ok_or(SegmentError::TooManyEntries)?;
            let length = u64::try_from(record.bytes.len()).map_err(|_| SegmentError::TooLarge)?;
            projected_payload = projected_payload
                .checked_add(length)
                .ok_or(SegmentError::TooLarge)?;
            validate_projected(projected_count, projected_payload)?;
            accepted_by_key.insert(record_key, accepted.len());
            accepted.push(*record);
        }
        if !retry && accepted.is_empty() {
            return Err(ActiveError::NoChanges);
        }

        // The recovery allocation is deliberately deferred until every record has
        // validated and the batch is known to perform a real append.
        let old_tail = if retry {
            Vec::new()
        } else {
            let old_tail_len = self
                .state
                .metadata
                .total_length
                .checked_sub(self.state.metadata.index_start)
                .ok_or(ActiveError::PublishedStateMismatch)?;
            if old_tail_len > MAX_SEGMENT_TAIL_BYTES {
                return Err(ActiveError::PublishedStateMismatch);
            }
            let old_tail_size =
                usize::try_from(old_tail_len).map_err(|_| ActiveError::PublishedStateMismatch)?;
            let mut old_tail = vec![0; old_tail_size];
            self.file
                .seek(SeekFrom::Start(self.state.metadata.index_start))?;
            self.file.read_exact(&mut old_tail)?;
            old_tail
        };
        Ok(PreparedAppend {
            old: self.state,
            intended_generation,
            records: accepted,
            journal: ActiveRecoveryJournal {
                old: self.state,
                intended_generation,
                old_tail,
            },
        })
    }

    /// Applies a prepared append. The caller owns journal durability and head
    /// publication; the returned state is the value to publish after syncing.
    pub(crate) fn commit_append(
        &mut self,
        prepared: PreparedAppend<'_>,
    ) -> Result<ActiveAppendResult, ActiveError> {
        if self.poisoned {
            return Err(ActiveError::Poisoned);
        }
        if self.state != prepared.old {
            return Err(ActiveError::StaleAppend);
        }
        if prepared.records.is_empty() {
            debug_assert_eq!(prepared.intended_generation, self.state.generation);
            return Ok(ActiveAppendResult {
                state: self.state,
                added_entries: Vec::new(),
            });
        }

        let added_keys: Vec<_> = prepared
            .records
            .iter()
            .map(|record| (record.namespace, record.digest))
            .collect();
        self.poisoned = true;
        let append_result = (|| -> Result<_, ActiveError> {
            self.file.set_len(self.state.metadata.index_start)?;
            self.file
                .seek(SeekFrom::Start(self.state.metadata.index_start))?;
            let mut payload_bytes = self.state.metadata.payload_bytes;
            for record in &prepared.records {
                let record = *record;
                let length =
                    u64::try_from(record.bytes.len()).map_err(|_| SegmentError::TooLarge)?;
                let offset = PREFIX_LEN
                    .checked_add(payload_bytes)
                    .ok_or(SegmentError::TooLarge)?;
                self.file.write_all(record.bytes)?;
                payload_bytes = payload_bytes
                    .checked_add(length)
                    .ok_or(SegmentError::TooLarge)?;
                self.entries.push(SegmentIndexEntry {
                    namespace: record.namespace,
                    kind: record.kind,
                    digest: record.digest,
                    offset,
                    length,
                });
            }
            #[cfg(test)]
            if self.fail_commit_after_payload {
                self.fail_commit_after_payload = false;
                return Err(std::io::Error::other("injected active append failure").into());
            }
            let index_start = PREFIX_LEN
                .checked_add(payload_bytes)
                .ok_or(SegmentError::TooLarge)?;
            let footer = emit_cumulative_tail(
                &mut self.file,
                &mut self.entries,
                index_start,
                &mut self.io_buffer,
            )?;
            Ok(ActivePublishedState {
                generation: prepared.intended_generation,
                id: footer.segment_id(),
                metadata: super::segment::metadata(&footer),
            })
        })();

        match append_result {
            Ok(state) => {
                self.state = state;
                self.poisoned = false;
                let added_entries = added_keys
                    .iter()
                    .map(|wanted| {
                        let index = self
                            .entries
                            .binary_search_by_key(wanted, super::segment::key)
                            .expect("committed entry must remain indexed");
                        self.entries[index].clone()
                    })
                    .collect();
                Ok(ActiveAppendResult {
                    state,
                    added_entries,
                })
            }
            Err(error) => {
                // Recovery authenticates a temporary candidate before touching
                // the damaged file. If recovery itself fails, poison remains set.
                if self.restore(&prepared.journal).is_ok() {
                    self.poisoned = false;
                }
                Err(error)
            }
        }
    }

    /// Restores the exact previously published tail and truncates any attempted
    /// payload/tail bytes. This is idempotent and validates a temporary image
    /// before making any destructive change to the active file.
    pub(crate) fn restore(&mut self, journal: &ActiveRecoveryJournal) -> Result<(), ActiveError> {
        if journal.old != self.state
            || journal.old.generation.checked_add(1) != Some(journal.intended_generation)
        {
            return Err(ActiveError::StaleAppend);
        }
        let expected_len = journal
            .old
            .metadata
            .total_length
            .checked_sub(journal.old.metadata.index_start)
            .ok_or(ActiveError::PublishedStateMismatch)?;
        if usize::try_from(expected_len).ok() != Some(journal.old_tail.len())
            || expected_len > MAX_SEGMENT_TAIL_BYTES
        {
            return Err(ActiveError::PublishedStateMismatch);
        }

        let mut candidate = tempfile::tempfile()?;
        self.file.seek(SeekFrom::Start(0))?;
        std::io::copy(
            &mut std::io::Read::by_ref(&mut self.file).take(journal.old.metadata.index_start),
            &mut candidate,
        )?;
        if candidate.stream_position()? != journal.old.metadata.index_start {
            return Err(ActiveError::PublishedStateMismatch);
        }
        candidate.write_all(&journal.old_tail)?;
        candidate.set_len(journal.old.metadata.total_length)?;
        let reader = SegmentReader::open(candidate.try_clone()?)?;
        if reader.id() != journal.old.id || reader.metadata() != journal.old.metadata {
            return Err(ActiveError::PublishedStateMismatch);
        }
        let (reader_file, _, _, entries) = reader.into_authenticated_parts();
        drop(reader_file);

        candidate.seek(SeekFrom::Start(journal.old.metadata.index_start))?;
        self.file.set_len(journal.old.metadata.index_start)?;
        self.file
            .seek(SeekFrom::Start(journal.old.metadata.index_start))?;
        std::io::copy(&mut candidate, &mut self.file)?;
        self.file.set_len(journal.old.metadata.total_length)?;
        self.entries = entries;
        self.state = journal.old;
        self.poisoned = false;
        Ok(())
    }

    pub(crate) fn sync(&self, durability: Durability) -> Result<(), ActiveError> {
        durability.sync_file(&self.file)?;
        Ok(())
    }

    pub(crate) fn entries(&self) -> &[SegmentIndexEntry] {
        &self.entries
    }

    /// Reads through the index already authenticated when this active segment
    /// was created, reopened, or last committed.
    pub(crate) fn lookup_entry_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(SegmentIndexEntry, Vec<u8>)>, ActiveError> {
        let wanted = (namespace, digest);
        let Ok(index) = self
            .entries
            .binary_search_by_key(&wanted, super::segment::key)
        else {
            return Ok(None);
        };
        let entry = self.entries[index].clone();
        let length = usize::try_from(entry.length).map_err(|_| SegmentError::ReadLimit {
            minimum: usize::MAX,
        })?;
        if length > max {
            return Err(SegmentError::ReadLimit { minimum: length }.into());
        }
        let mut bytes = vec![0; length];
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.read_exact(&mut bytes)?;
        super::segment::verify_logical(&entry, &bytes)?;
        Ok(Some((entry, bytes)))
    }

    #[cfg(test)]
    pub(crate) fn sealing_metadata(&self) -> ActiveSealMetadata {
        ActiveSealMetadata {
            id: self.state.id,
            stored_length: self.state.metadata.total_length,
            object_count: self.state.metadata.object_count,
        }
    }

    pub(crate) fn should_seal(&self) -> bool {
        self.state.metadata.total_length >= ACTIVE_SEGMENT_TARGET_BYTES
            || self.entries.len() >= MAX_SEGMENT_ENTRIES
    }

    fn find_entry(&self, namespace: u8, digest: [u8; 32]) -> Option<(u16, u64, u64)> {
        let wanted = (namespace, digest);
        self.entries
            .binary_search_by_key(&wanted, super::segment::key)
            .ok()
            .map(|index| {
                let entry = &self.entries[index];
                (entry.kind, entry.offset, entry.length)
            })
    }

    fn payload_equals(
        &mut self,
        offset: u64,
        length: u64,
        proposed: &[u8],
    ) -> Result<bool, ActiveError> {
        if length != proposed.len() as u64 {
            return Ok(false);
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut consumed = 0;
        while consumed < proposed.len() {
            let amount = (proposed.len() - consumed).min(self.io_buffer.len());
            self.file.read_exact(&mut self.io_buffer[..amount])?;
            if self.io_buffer[..amount] != proposed[consumed..consumed + amount] {
                return Ok(false);
            }
            consumed += amount;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use wheat_core::ContentId;

    use super::*;
    use crate::packed::segment::{CONTENT_NAMESPACE, MAX_INDEX_PAGE_ENTRIES};

    fn record(bytes: &[u8]) -> SegmentRecord<'_> {
        SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: *ContentId::of(bytes).as_bytes(),
            bytes,
        }
    }

    fn append(
        active: &mut ActiveSegment,
        records: &[SegmentRecord<'_>],
    ) -> (ActiveAppendResult, ActiveRecoveryJournal) {
        let generation = active.state().generation + 1;
        let prepared = active.prepare_append(records, generation).unwrap();
        let journal = prepared.journal().clone();
        let result = active.commit_append(prepared).unwrap();
        (result, journal)
    }

    fn authenticated_entries(active: &ActiveSegment) -> Vec<SegmentIndexEntry> {
        let reader = SegmentReader::open(active.file.try_clone().unwrap()).unwrap();
        let (_, _, _, entries) = reader.into_authenticated_parts();
        entries
    }

    #[test]
    fn multiple_appends_preserve_payload_bytes_and_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();

        let first = b"first payload";
        let second = b"second payload";
        append(&mut active, &[record(first)]);
        let first_state = active.state();
        let first_payload = fs::read(&path).unwrap()
            [PREFIX_LEN as usize..first_state.metadata.index_start as usize]
            .to_vec();
        let first_entry = authenticated_entries(&active).remove(0);

        append(&mut active, &[record(second)]);
        let entries = authenticated_entries(&active);
        let retained = entries
            .iter()
            .find(|entry| entry.digest == *ContentId::of(first).as_bytes())
            .unwrap();
        let added = entries
            .iter()
            .find(|entry| entry.digest == *ContentId::of(second).as_bytes())
            .unwrap();
        assert_eq!(retained.offset, first_entry.offset);
        assert_eq!(retained.length, first_entry.length);
        assert_eq!(added.offset, first_state.metadata.index_start);

        let bytes = fs::read(path).unwrap();
        assert_eq!(
            &bytes[PREFIX_LEN as usize..PREFIX_LEN as usize + first_payload.len()],
            first_payload
        );
        assert_eq!(
            &bytes[added.offset as usize..(added.offset + added.length) as usize],
            second
        );
    }

    #[test]
    fn cumulative_index_is_sorted_across_page_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 10).unwrap();
        let values: Vec<Vec<u8>> = (0..MAX_INDEX_PAGE_ENTRIES + 5)
            .rev()
            .map(|number| number.to_be_bytes().to_vec())
            .collect();
        let records: Vec<_> = values.iter().map(|value| record(value)).collect();
        append(&mut active, &records);

        let entries = authenticated_entries(&active);
        assert_eq!(entries.len(), values.len());
        assert!(entries.windows(2).all(|pair| {
            (pair[0].namespace, pair[0].digest) < (pair[1].namespace, pair[1].digest)
        }));
        let raw = fs::read(&path).unwrap();
        let footer = wheat_core::decode_segment_footer(
            &raw[raw.len() - wheat_core::SEGMENT_FOOTER_ENCODED_SIZE..],
        )
        .unwrap();
        let directory = wheat_core::decode_segment_directory(
            &raw[footer.directory_offset as usize
                ..(footer.directory_offset + footer.directory_length) as usize],
        )
        .unwrap();
        assert!(directory.pages.len() >= 2);
    }

    #[test]
    fn duplicate_batches_are_retries_and_conflicts_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(path, 0).unwrap();
        let value = b"same";
        append(&mut active, &[record(value)]);
        let state = active.state();

        let retry = active
            .prepare_append(&[record(value)], state.generation)
            .unwrap();
        assert!(retry.is_retry());
        let result = active.commit_append(retry).unwrap();
        assert!(result.added_entries.is_empty());
        assert_eq!(result.state, state);

        let digest = *ContentId::of(value).as_bytes();
        let conflicting = SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest,
            bytes: b"different",
        };
        assert!(matches!(
            active.prepare_append(&[conflicting], state.generation + 1),
            Err(ActiveError::Segment(SegmentError::ObjectDigest))
        ));
        assert!(matches!(
            active.prepare_append(&[record(b"new")], 4),
            Err(ActiveError::Generation)
        ));

        let first = SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: [9; 32],
            bytes: b"a",
        };
        let second = SegmentRecord {
            bytes: b"b",
            ..first
        };
        assert!(matches!(
            active.prepare_append(&[first, second], state.generation + 1),
            Err(ActiveError::Segment(SegmentError::ObjectDigest))
        ));
    }

    #[test]
    fn explicit_count_payload_and_recovery_tail_bounds_are_enforced() {
        assert!(matches!(
            validate_projected(MAX_SEGMENT_ENTRIES + 1, 0),
            Err(SegmentError::TooManyEntries)
        ));
        assert!(matches!(
            validate_projected(1, MAX_SEGMENT_PAYLOAD_BYTES + 1),
            Err(SegmentError::TooLarge)
        ));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(path, 0).unwrap();
        let prepared = active.prepare_append(&[record(b"x")], 1).unwrap();
        let mut journal = prepared.journal().clone();
        journal.old_tail.push(0);
        assert!(matches!(
            active.restore(&journal),
            Err(ActiveError::PublishedStateMismatch)
        ));
    }

    #[test]
    fn commit_failure_after_payload_restores_exact_authenticated_state_and_remains_usable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 7).unwrap();
        append(&mut active, &[record(b"published payload")]);
        let old_state = active.state();
        let old_entries = active.entries().to_vec();
        let old_bytes = fs::read(&path).unwrap();

        active.test_fail_commit_after_payload();
        let prepared = active
            .prepare_append(&[record(b"payload written before failure")], 9)
            .unwrap();
        assert!(matches!(
            active.commit_append(prepared),
            Err(ActiveError::Io(_))
        ));
        assert_eq!(active.state(), old_state);
        assert_eq!(active.entries(), old_entries);
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        let reader = SegmentReader::open(active.file.try_clone().unwrap()).unwrap();
        assert_eq!(reader.id(), old_state.id);
        assert_eq!(reader.metadata(), old_state.metadata);

        append(&mut active, &[record(b"usable afterward")]);
        assert_eq!(active.state().generation, 9);
        assert!(
            active
                .lookup_entry_bounded(
                    CONTENT_NAMESPACE,
                    *ContentId::of(b"usable afterward").as_bytes(),
                    usize::MAX,
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn exact_old_tail_restores_after_append_and_reopen_ignores_suffix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"old")]);
        let old_state = active.state();
        let old_bytes = fs::read(&path).unwrap();

        let prepared = active.prepare_append(&[record(b"new")], 2).unwrap();
        let journal = prepared.journal().clone();
        active.commit_append(prepared).unwrap();
        active.file.seek(SeekFrom::End(0)).unwrap();
        active.file.write_all(b"partial-unpublished-tail").unwrap();
        drop(active);
        let mut active = ActiveSegment::recover(&path, &journal).unwrap();
        assert_eq!(active.state(), old_state);
        assert_eq!(fs::read(&path).unwrap(), old_bytes);

        active.file.seek(SeekFrom::End(0)).unwrap();
        active.file.write_all(b"ignored suffix").unwrap();
        drop(active);
        let reopened = ActiveSegment::reopen(&path, old_state).unwrap();
        assert_eq!(reopened.state(), old_state);
        assert!(fs::metadata(&path).unwrap().len() > old_state.metadata.total_length);

        let mut wrong = old_state;
        wrong.generation += 1;
        wrong.id = SegmentId([7; 32]);
        assert!(matches!(
            ActiveSegment::reopen(path, wrong),
            Err(ActiveError::PublishedStateMismatch)
        ));
    }

    #[test]
    fn sealing_metadata_describes_the_current_v1_segment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(path, 4).unwrap();
        append(&mut active, &[record(b"payload")]);
        active.sync(crate::Durability::Durable).unwrap();
        let state = active.state();
        assert_eq!(
            active.sealing_metadata(),
            ActiveSealMetadata {
                id: state.id,
                stored_length: state.metadata.total_length,
                object_count: 1,
            }
        );
        assert!(!active.should_seal());
    }
}
