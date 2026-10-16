//! Process-local mutable segment growth, as an append-only log.
//!
//! The active segment is a header followed by one frame per durable append: the
//! frame header, the new payloads, an index page of their entries, and a
//! trailer whose chain digest covers the previous chain digest and the frame.
//! An append only writes past the end of the file, so the bytes of every
//! earlier frame stay as they were, and the newest frame whose chain verifies
//! is the published state: no separate head or recovery journal exists.
//! Sealing rebuilds the records into an immutable V1 segment.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use wheat_core::{
    ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE, ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE,
    ACTIVE_LOG_HEADER_ENCODED_SIZE, ActiveLogFrameFields, SEGMENT_INDEX_ENTRY_ENCODED_SIZE,
    SegmentId, SegmentIndexEntry, SegmentIndexPage, decode_active_log_frame_header,
    decode_active_log_frame_trailer, decode_active_log_header, decode_segment_index_page,
    encode_active_log_frame_fields, encode_active_log_frame_header,
    encode_active_log_frame_trailer, encode_active_log_header, encode_segment_index_page,
};

#[cfg(test)]
use super::segment::prepare_record;
use super::segment::{
    MAX_SEGMENT_ENTRIES, MAX_SEGMENT_PAYLOAD_BYTES, SegmentError, SegmentRecord, key,
};
use crate::{Durability, codec::Codec};

const HEADER_LEN: u64 = ACTIVE_LOG_HEADER_ENCODED_SIZE as u64;
const FRAME_HEADER_LEN: u64 = ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE as u64;
const FRAME_TRAILER_LEN: u64 = ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE as u64;
/// The encoded fields lead the frame header; its last 32 bytes are their digest.
const FRAME_FIELDS_LEN: usize = ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE - 32;
/// The largest index page a frame can hold: every entry of a full segment.
const MAX_FRAME_INDEX_BYTES: u64 =
    4 + (MAX_SEGMENT_ENTRIES * SEGMENT_INDEX_ENTRY_ENCODED_SIZE) as u64;
const IO_BUFFER_SIZE: usize = 64 * 1024;
pub(crate) const ACTIVE_SEGMENT_TARGET_BYTES: u64 = 128 * 1024 * 1024;

/// The state the newest verified frame publishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActivePublishedState {
    /// The log's base generation plus its number of frames.
    pub generation: u64,
    /// The newest chain digest.
    pub id: SegmentId,
    pub object_count: u64,
    /// Stored bytes of the records, without the log's own framing.
    pub payload_bytes: u64,
    /// Length of the log up to the end of the newest frame.
    pub total_length: u64,
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
    #[error("active segment log is corrupt: {0}")]
    Corrupt(&'static str),
    /// The log's own header never reached the disk: creation was interrupted.
    #[error("active segment log has no complete header")]
    Unwritten,
    #[error("active-segment generation must increase by exactly one")]
    Generation,
    #[error("existing segment key differs from proposed bytes")]
    ConflictingKey,
    #[error("prepared append no longer applies to this active segment")]
    StaleAppend,
    #[error("active segment is unusable until it is reopened")]
    Poisoned,
    #[error("an append must add at least one new record")]
    NoChanges,
}

pub(crate) struct PreparedAppend<'a> {
    old: ActivePublishedState,
    intended_generation: u64,
    records: Vec<(SegmentRecord<'a>, crate::codec::PhysicalPayload)>,
}

impl PreparedAppend<'_> {
    #[cfg(test)]
    pub(crate) fn is_retry(&self) -> bool {
        self.records.is_empty()
    }
}

/// The file writes of one append, made through a handle of their own so they need
/// no access to the [`ActiveSegment`]. Meanwhile the segment keeps serving its
/// published records: the frame starts where the published log ends, and the
/// segment's entries change only in [`ActiveSegment::finish_append`].
pub(crate) struct ActiveWrite<'a> {
    file: File,
    prepared: PreparedAppend<'a>,
    io_buffer: Vec<u8>,
    #[cfg(test)]
    fail_after_payload: bool,
}

/// What an [`ActiveWrite`] did, for [`ActiveSegment::finish_append`].
pub(crate) struct ActiveWritten {
    file: File,
    /// The state the frame publishes and its entries in key order.
    result: Result<(ActivePublishedState, Vec<SegmentIndexEntry>), ActiveError>,
    old: ActivePublishedState,
    io_buffer: Vec<u8>,
}

impl ActiveWrite<'_> {
    /// Writes one frame after the published log.
    pub(crate) fn run(mut self) -> ActiveWritten {
        let result = self.write();
        ActiveWritten {
            file: self.file,
            result,
            old: self.prepared.old,
            io_buffer: self.io_buffer,
        }
    }

    fn write(&mut self) -> Result<(ActivePublishedState, Vec<SegmentIndexEntry>), ActiveError> {
        let old = self.prepared.old;
        // Bytes past the published log belong to no frame: a failed append that
        // could not truncate them poisons the segment before another append.
        self.file.set_len(old.total_length)?;
        self.file.seek(SeekFrom::Start(old.total_length))?;

        let payload_start = old
            .total_length
            .checked_add(FRAME_HEADER_LEN)
            .ok_or(SegmentError::TooLarge)?;
        let mut payload_length = 0_u64;
        let mut entries = Vec::with_capacity(self.prepared.records.len());
        for (record, physical) in &self.prepared.records {
            let stored_length = physical.bytes.len() as u64;
            entries.push(SegmentIndexEntry {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                offset: payload_start
                    .checked_add(payload_length)
                    .ok_or(SegmentError::TooLarge)?,
                codec: physical.codec.to_byte(),
                stored_length,
                logical_length: physical.logical_length,
                delta_base: None,
                delta_depth: 0,
            });
            payload_length = payload_length
                .checked_add(stored_length)
                .ok_or(SegmentError::TooLarge)?;
        }
        entries.sort_unstable_by_key(key);
        let page = SegmentIndexPage { entries };
        let index = encode_segment_index_page(&page).map_err(SegmentError::from)?;
        let entries = page.entries;

        let fields = ActiveLogFrameFields {
            generation: self.prepared.intended_generation,
            payload_length,
            index_length: index.len() as u64,
        };
        let header = encode_active_log_frame_header(&fields, fields_digest(old.id, &fields)?)
            .map_err(SegmentError::from)?;
        let mut chain = blake3::Hasher::new();
        chain.update(&old.id.0);
        let mut writer = FrameWriter {
            file: &mut self.file,
            buffer: &mut self.io_buffer,
            chain: &mut chain,
        };
        writer.buffer.clear();
        writer.write(&header)?;
        for (_, physical) in &self.prepared.records {
            writer.write(&physical.bytes)?;
        }
        #[cfg(test)]
        if self.fail_after_payload {
            writer.flush()?;
            return Err(std::io::Error::other("injected active append failure").into());
        }
        writer.write(&index)?;
        let chain_digest = *chain.finalize().as_bytes();
        let trailer = encode_active_log_frame_trailer(chain_digest).map_err(SegmentError::from)?;
        self.file.write_all(&self.io_buffer)?;
        self.file.write_all(&trailer)?;
        self.io_buffer.clear();

        let total_length = [FRAME_HEADER_LEN, payload_length, fields.index_length]
            .into_iter()
            .chain([FRAME_TRAILER_LEN])
            .try_fold(old.total_length, u64::checked_add)
            .ok_or(SegmentError::TooLarge)?;
        let state = ActivePublishedState {
            generation: fields.generation,
            id: SegmentId(chain_digest),
            object_count: old.object_count + entries.len() as u64,
            payload_bytes: old.payload_bytes + payload_length,
            total_length,
        };
        Ok((state, entries))
    }
}

/// Buffers a frame's small writes into one and feeds every byte to the chain.
struct FrameWriter<'a> {
    file: &'a mut File,
    buffer: &'a mut Vec<u8>,
    chain: &'a mut blake3::Hasher,
}

impl FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.chain.update(bytes);
        if self.buffer.len() + bytes.len() > IO_BUFFER_SIZE {
            self.flush()?;
        }
        if bytes.len() >= IO_BUFFER_SIZE {
            self.file.write_all(bytes)
        } else {
            self.buffer.extend_from_slice(bytes);
            Ok(())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.write_all(self.buffer)?;
        self.buffer.clear();
        Ok(())
    }
}

/// The digest a frame header carries over its encoded fields.
fn fields_digest(
    previous: SegmentId,
    fields: &ActiveLogFrameFields,
) -> Result<[u8; 32], SegmentError> {
    let encoded = encode_active_log_frame_fields(fields)?;
    Ok(fields_digest_of(previous, &encoded))
}

fn fields_digest_of(previous: SegmentId, encoded_fields: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous.0);
    hasher.update(encoded_fields);
    *hasher.finalize().as_bytes()
}

impl ActiveWritten {
    /// The state and the index entries the write added, when it succeeded.
    pub(crate) fn appended(&self) -> Option<(ActivePublishedState, &[SegmentIndexEntry])> {
        let (state, entries) = self.result.as_ref().ok()?;
        Some((*state, entries))
    }

    /// Makes the written frame durable. A failed sync fails the write, which
    /// [`ActiveSegment::finish_append`] then removes.
    pub(crate) fn sync(&mut self, durability: Durability) {
        if self.result.is_ok()
            && let Err(error) = durability.sync_file(&self.file)
        {
            self.result = Err(error.into());
        }
    }

    #[cfg(test)]
    pub(crate) fn test_fail(&mut self) {
        self.result = Err(std::io::Error::other("injected active sync failure").into());
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

/// A log read back from disk: its newest verified state and every entry in key
/// order, and the length of the file the frames end at.
struct Scanned {
    state: ActivePublishedState,
    entries: Vec<SegmentIndexEntry>,
}

pub(crate) struct ActiveSegment {
    path: PathBuf,
    file: File,
    state: ActivePublishedState,
    entries: Vec<SegmentIndexEntry>,
    io_buffer: Vec<u8>,
    poisoned: bool,
    #[cfg(test)]
    fail_commit_after_payload: bool,
}

impl ActiveSegment {
    /// Creates a new, empty log whose state is at `generation`.
    pub(crate) fn create(path: impl AsRef<Path>, generation: u64) -> Result<Self, ActiveError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let header = encode_active_log_header(generation).map_err(SegmentError::from)?;
        file.write_all(&header)?;
        let state = ActivePublishedState {
            generation,
            id: SegmentId(*blake3::hash(&header).as_bytes()),
            object_count: 0,
            payload_bytes: 0,
            total_length: HEADER_LEN,
        };
        Ok(Self::new(path, file, state, Vec::new()))
    }

    fn new(
        path: PathBuf,
        file: File,
        state: ActivePublishedState,
        entries: Vec<SegmentIndexEntry>,
    ) -> Self {
        Self {
            path,
            file,
            state,
            entries,
            io_buffer: Vec::with_capacity(IO_BUFFER_SIZE),
            poisoned: false,
            #[cfg(test)]
            fail_commit_after_payload: false,
        }
    }

    /// Records that the segment's file was renamed to `path`, where appends
    /// reopen it.
    pub(crate) fn renamed(&mut self, path: impl AsRef<Path>) {
        self.path = path.as_ref().to_path_buf();
    }

    /// Opens the log for appending at its newest verified frame and truncates the
    /// bytes of an append a crash interrupted. Returns whether it truncated any,
    /// which the caller makes durable with [`Self::sync`].
    pub(crate) fn recover(path: impl AsRef<Path>) -> Result<(Self, bool), ActiveError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let length = file.metadata()?.len();
        let Scanned { state, entries } = scan(&mut file, length)?;
        let torn = state.total_length != length;
        if torn {
            file.set_len(state.total_length)?;
        }
        Ok((Self::new(path, file, state, entries), torn))
    }

    /// Opens the log read-only at its newest verified frame, leaving the bytes of
    /// an interrupted append in place.
    pub(crate) fn open_read_only(path: impl AsRef<Path>) -> Result<Self, ActiveError> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let length = file.metadata()?.len();
        let Scanned { state, entries } = scan(&mut file, length)?;
        Ok(Self::new(path, file, state, entries))
    }

    pub(crate) fn state(&self) -> ActivePublishedState {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn test_fail_commit_after_payload(&mut self) {
        self.fail_commit_after_payload = true;
    }

    /// Validates and deduplicates a bounded batch. No segment bytes are changed
    /// by this method.
    #[cfg(test)]
    pub(crate) fn prepare_append<'a>(
        &mut self,
        records: &[SegmentRecord<'a>],
        intended_generation: u64,
    ) -> Result<PreparedAppend<'a>, ActiveError> {
        let prepared = records
            .iter()
            .copied()
            .map(prepare_record)
            .collect::<Result<Vec<_>, _>>()?;
        self.prepare_prepared_append(prepared, intended_generation)
    }

    pub(crate) fn prepare_prepared_append<'a>(
        &mut self,
        records: Vec<super::segment::PreparedSegmentRecord<'a>>,
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
        let mut projected_payload = self.state.payload_bytes;
        for prepared in records {
            let record = prepared.logical;
            if let Some(index) = self.find_entry(record.namespace, record.digest) {
                if self.entries[index].kind != record.kind
                    || !self.payload_equals(index, record.bytes)?
                {
                    return Err(ActiveError::ConflictingKey);
                }
                continue;
            }
            if retry {
                return Err(ActiveError::StaleAppend);
            }
            let record_key = (record.namespace, record.digest);
            if let Some(&index) = accepted_by_key.get(&record_key) {
                let existing: &(SegmentRecord<'_>, crate::codec::PhysicalPayload) =
                    &accepted[index];
                if existing.0.kind != record.kind || existing.0.bytes != record.bytes {
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
            projected_payload = projected_payload
                .checked_add(prepared.stored_length())
                .ok_or(SegmentError::TooLarge)?;
            validate_projected(projected_count, projected_payload)?;
            accepted_by_key.insert(record_key, accepted.len());
            accepted.push((record, prepared.physical));
        }
        if !retry && accepted.is_empty() {
            return Err(ActiveError::NoChanges);
        }
        Ok(PreparedAppend {
            old: self.state,
            intended_generation,
            records: accepted,
        })
    }

    /// Applies a prepared append without syncing it.
    #[cfg(test)]
    pub(crate) fn commit_append(
        &mut self,
        prepared: PreparedAppend<'_>,
    ) -> Result<ActiveAppendResult, ActiveError> {
        match self.begin_append(prepared)? {
            Ok(write) => self.finish_append(write.run()),
            Err(unchanged) => Ok(unchanged),
        }
    }

    /// Starts applying a prepared append: the writes it needs, or the unchanged
    /// result of a retry that adds nothing. The segment refuses other appends until
    /// [`Self::finish_append`] gets the writes' outcome; until then it serves its
    /// published records as before.
    pub(crate) fn begin_append<'a>(
        &mut self,
        prepared: PreparedAppend<'a>,
    ) -> Result<Result<ActiveWrite<'a>, ActiveAppendResult>, ActiveError> {
        if self.poisoned {
            return Err(ActiveError::Poisoned);
        }
        if self.state != prepared.old {
            return Err(ActiveError::StaleAppend);
        }
        if prepared.records.is_empty() {
            debug_assert_eq!(prepared.intended_generation, self.state.generation);
            return Ok(Err(ActiveAppendResult {
                state: self.state,
                added_entries: Vec::new(),
            }));
        }
        // A handle of its own: readers seek this segment's handle meanwhile.
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        self.poisoned = true;
        Ok(Ok(ActiveWrite {
            file,
            prepared,
            io_buffer: std::mem::take(&mut self.io_buffer),
            #[cfg(test)]
            fail_after_payload: std::mem::take(&mut self.fail_commit_after_payload),
        }))
    }

    /// Publishes the records an [`ActiveWrite`] added, or truncates the frame it
    /// failed to write.
    pub(crate) fn finish_append(
        &mut self,
        written: ActiveWritten,
    ) -> Result<ActiveAppendResult, ActiveError> {
        let ActiveWritten {
            result,
            old,
            io_buffer,
            ..
        } = written;
        self.io_buffer = io_buffer;
        if self.state != old {
            // Only the segment that began the append may finish it.
            return Err(ActiveError::StaleAppend);
        }
        match result {
            Ok((state, added_entries)) => {
                // Both runs are in key order, which the stable sort merges in
                // linear time.
                self.entries.extend_from_slice(&added_entries);
                self.entries.sort_by_key(key);
                self.state = state;
                self.poisoned = false;
                Ok(ActiveAppendResult {
                    state,
                    added_entries,
                })
            }
            Err(error) => {
                // The published frames are untouched; only the failed frame's
                // bytes go. If even that fails, the segment stays poisoned.
                if self.file.set_len(old.total_length).is_ok() {
                    self.poisoned = false;
                }
                Err(error)
            }
        }
    }

    pub(crate) fn sync(&self, durability: Durability) -> Result<(), ActiveError> {
        durability.sync_file(&self.file)?;
        Ok(())
    }

    pub(crate) fn entries(&self) -> &[SegmentIndexEntry] {
        &self.entries
    }

    /// The authenticated index entry for a key, without reading its payload.
    pub(crate) fn entry(&self, namespace: u8, digest: [u8; 32]) -> Option<&SegmentIndexEntry> {
        self.find_entry(namespace, digest)
            .map(|index| &self.entries[index])
    }

    /// Reads through the index verified when this active segment was created,
    /// recovered, or last appended to, and verifies the record's bytes.
    pub(crate) fn lookup_entry_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(SegmentIndexEntry, Vec<u8>)>, ActiveError> {
        let Some(index) = self.find_entry(namespace, digest) else {
            return Ok(None);
        };
        let entry = self.entries[index].clone();
        let logical_length =
            usize::try_from(entry.logical_length).map_err(|_| SegmentError::ReadLimit {
                minimum: usize::MAX,
            })?;
        if logical_length > max {
            return Err(SegmentError::ReadLimit {
                minimum: logical_length,
            }
            .into());
        }
        let stored_length = usize::try_from(entry.stored_length)
            .map_err(|_| SegmentError::Format("stored length"))?;
        let mut stored = vec![0; stored_length];
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.read_exact(&mut stored)?;
        let codec = Codec::from_byte(entry.codec).map_err(SegmentError::from)?;
        let bytes = crate::codec::decode(codec, stored, entry.logical_length, max)
            .map_err(SegmentError::from)?;
        super::segment::verify_logical(&entry, &bytes)?;
        Ok(Some((entry, bytes)))
    }

    #[cfg(test)]
    pub(crate) fn sealing_metadata(&self) -> ActiveSealMetadata {
        ActiveSealMetadata {
            id: self.state.id,
            stored_length: self.state.total_length,
            object_count: self.state.object_count,
        }
    }

    pub(crate) fn should_seal(&self) -> bool {
        self.state.total_length >= ACTIVE_SEGMENT_TARGET_BYTES
            || self.entries.len() >= MAX_SEGMENT_ENTRIES
    }

    fn find_entry(&self, namespace: u8, digest: [u8; 32]) -> Option<usize> {
        self.entries
            .binary_search_by_key(&(namespace, digest), key)
            .ok()
    }

    fn payload_equals(&mut self, index: usize, proposed: &[u8]) -> Result<bool, ActiveError> {
        let entry = self.entries[index].clone();
        if entry.logical_length != proposed.len() as u64 {
            return Ok(false);
        }
        let Some((_, actual)) =
            self.lookup_entry_bounded(entry.namespace, entry.digest, proposed.len())?
        else {
            return Ok(false);
        };
        Ok(actual == proposed)
    }
}

/// Reads the log's frames in order, verifying each one's chain digest and index.
///
/// Appends are sequential and each is synced before the next begins, so a crash
/// can damage only the last frame, and only by leaving its bytes unwritten: the
/// file ends inside it, or reads back zeros where the write did not land. Such
/// a frame ends the log. A frame that fails verification while a later frame
/// follows it, or whose header is damaged by anything but missing bytes, was
/// damaged after it was synced, and the log is corrupt.
fn scan(file: &mut File, length: u64) -> Result<Scanned, ActiveError> {
    if length < HEADER_LEN {
        return Err(ActiveError::Unwritten);
    }
    let mut header = [0; ACTIVE_LOG_HEADER_ENCODED_SIZE];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    let base_generation = match decode_active_log_header(&header) {
        Ok(generation) => generation,
        // Creation writes the header first and syncs it before any append.
        Err(_) if header.iter().all(|byte| *byte == 0) => return Err(ActiveError::Unwritten),
        Err(_) => return Err(ActiveError::Corrupt("log header")),
    };
    let mut state = ActivePublishedState {
        generation: base_generation,
        id: SegmentId(*blake3::hash(&header).as_bytes()),
        object_count: 0,
        payload_bytes: 0,
        total_length: HEADER_LEN,
    };
    let mut entries = Vec::new();
    let mut buffer = vec![0; IO_BUFFER_SIZE];
    let mut regions = Vec::new();
    loop {
        let start = state.total_length;
        let remaining = length - start;
        if remaining == 0 {
            break;
        }
        if remaining < FRAME_HEADER_LEN {
            break;
        }
        let mut frame_header = [0; ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut frame_header)?;
        let decoded = decode_active_log_frame_header(&frame_header)
            .ok()
            .filter(|(_, digest)| {
                *digest == fields_digest_of(state.id, &frame_header[..FRAME_FIELDS_LEN])
            });
        let Some((fields, _)) = decoded else {
            // The digest ends the header, so a header the write did not finish
            // ends in zeros.
            if frame_header.last() == Some(&0) {
                break;
            }
            return Err(ActiveError::Corrupt("frame header"));
        };
        // An authentic header's lengths are the ones the append wrote.
        if state.generation.checked_add(1) != Some(fields.generation) {
            return Err(ActiveError::Corrupt("frame generation"));
        }
        let payload_bytes = state
            .payload_bytes
            .checked_add(fields.payload_length)
            .filter(|bytes| *bytes <= MAX_SEGMENT_PAYLOAD_BYTES)
            .ok_or(ActiveError::Corrupt("frame payload length"))?;
        if fields.index_length > MAX_FRAME_INDEX_BYTES {
            return Err(ActiveError::Corrupt("frame index length"));
        }
        let payload_start = start + FRAME_HEADER_LEN;
        let index_start = payload_start + fields.payload_length;
        let trailer_start = index_start + fields.index_length;
        let end = trailer_start + FRAME_TRAILER_LEN;
        if end > length {
            break;
        }

        let mut chain = blake3::Hasher::new();
        chain.update(&state.id.0);
        chain.update(&frame_header);
        hash_range(
            file,
            payload_start,
            fields.payload_length,
            &mut chain,
            &mut buffer,
        )?;
        let mut index = vec![0; fields.index_length as usize];
        file.read_exact(&mut index)?;
        chain.update(&index);
        let mut trailer = [0; ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE];
        file.read_exact(&mut trailer)?;
        let chain_digest = *chain.finalize().as_bytes();
        if decode_active_log_frame_trailer(&trailer).ok() != Some(chain_digest) {
            if end == length {
                break;
            }
            return Err(ActiveError::Corrupt("frame chain digest"));
        }

        let page = decode_segment_index_page(&index).map_err(SegmentError::from)?;
        validate_frame_entries(
            &page.entries,
            payload_start,
            fields.payload_length,
            &mut regions,
        )?;
        let object_count = state.object_count + page.entries.len() as u64;
        entries.extend(page.entries);
        if entries.len() > MAX_SEGMENT_ENTRIES {
            return Err(SegmentError::TooManyEntries.into());
        }
        state = ActivePublishedState {
            generation: fields.generation,
            id: SegmentId(chain_digest),
            object_count,
            payload_bytes,
            total_length: end,
        };
    }
    entries.sort_by_key(key);
    if entries
        .windows(2)
        .any(|pair| key(&pair[0]) == key(&pair[1]))
    {
        return Err(SegmentError::DuplicateKey.into());
    }
    Ok(Scanned { state, entries })
}

/// Checks that a frame's entries are whole records in key order whose stored
/// bytes cover its payload exactly.
fn validate_frame_entries(
    entries: &[SegmentIndexEntry],
    payload_start: u64,
    payload_length: u64,
    regions: &mut Vec<(u64, u64)>,
) -> Result<(), ActiveError> {
    if entries.is_empty() {
        return Err(ActiveError::Corrupt("empty frame"));
    }
    if entries
        .windows(2)
        .any(|pair| key(&pair[0]) >= key(&pair[1]))
    {
        return Err(ActiveError::Corrupt("frame index order"));
    }
    regions.clear();
    for entry in entries {
        let codec = Codec::from_byte(entry.codec).map_err(SegmentError::from)?;
        if !matches!(codec, Codec::Raw | Codec::Zstd)
            || entry.logical_length > crate::codec::HARD_LOGICAL_RECORD_BYTES as u64
        {
            return Err(ActiveError::Corrupt("frame entry"));
        }
        regions.push((entry.offset, entry.stored_length));
    }
    regions.sort_unstable();
    let mut next = payload_start;
    for &(offset, stored_length) in regions.iter() {
        if offset != next {
            return Err(ActiveError::Corrupt("frame payload coverage"));
        }
        next = offset
            .checked_add(stored_length)
            .ok_or(ActiveError::Corrupt("frame payload coverage"))?;
    }
    if next != payload_start + payload_length {
        return Err(ActiveError::Corrupt("frame payload coverage"));
    }
    Ok(())
}

fn hash_range(
    file: &mut File,
    offset: u64,
    length: u64,
    hasher: &mut blake3::Hasher,
    buffer: &mut [u8],
) -> Result<(), ActiveError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    while remaining != 0 {
        let amount = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..amount])?;
        hasher.update(&buffer[..amount]);
        remaining -= amount as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use wheat_core::ContentId;

    use super::*;
    use crate::packed::segment::CONTENT_NAMESPACE;

    fn record(bytes: &[u8]) -> SegmentRecord<'_> {
        SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: *ContentId::of(bytes).as_bytes(),
            bytes,
        }
    }

    fn append(active: &mut ActiveSegment, records: &[SegmentRecord<'_>]) -> ActiveAppendResult {
        let generation = active.state().generation + 1;
        let prepared = active.prepare_append(records, generation).unwrap();
        active.commit_append(prepared).unwrap()
    }

    fn recovered(path: &Path) -> (ActiveSegment, bool) {
        ActiveSegment::recover(path).unwrap()
    }

    fn read(active: &mut ActiveSegment, bytes: &[u8]) -> Option<Vec<u8>> {
        active
            .lookup_entry_bounded(
                CONTENT_NAMESPACE,
                *ContentId::of(bytes).as_bytes(),
                usize::MAX,
            )
            .unwrap()
            .map(|(_, bytes)| bytes)
    }

    #[test]
    fn appends_only_extend_the_file_and_recover_to_the_same_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 3).unwrap();
        let created = fs::read(&path).unwrap();

        append(&mut active, &[record(b"first payload")]);
        let first = fs::read(&path).unwrap();
        assert!(first.starts_with(&created));
        let values: Vec<Vec<u8>> = (0_u32..300).map(|n| n.to_be_bytes().to_vec()).collect();
        let records: Vec<_> = values.iter().map(|value| record(value)).collect();
        append(&mut active, &records);
        let second = fs::read(&path).unwrap();
        assert!(second.starts_with(&first));

        let state = active.state();
        assert_eq!(state.generation, 5);
        assert_eq!(state.object_count, 301);
        assert_eq!(state.total_length, second.len() as u64);
        assert!(
            active
                .entries()
                .windows(2)
                .all(|pair| key(&pair[0]) < key(&pair[1]))
        );
        let entries = active.entries().to_vec();
        drop(active);

        let (mut reopened, torn) = recovered(&path);
        assert!(!torn);
        assert_eq!(reopened.state(), state);
        assert_eq!(reopened.entries(), entries);
        assert_eq!(
            read(&mut reopened, b"first payload").unwrap(),
            b"first payload"
        );
        assert_eq!(read(&mut reopened, &values[299]).unwrap(), values[299]);
    }

    #[test]
    fn a_frame_cut_short_anywhere_recovers_the_previous_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"durable")]);
        let durable = active.state();
        append(&mut active, &[record(b"torn"), record(b"also torn")]);
        let whole = fs::read(&path).unwrap();
        drop(active);

        for cut in durable.total_length..whole.len() as u64 {
            fs::write(&path, &whole[..cut as usize]).unwrap();
            let (mut reopened, torn) = recovered(&path);
            assert_eq!(torn, cut != durable.total_length, "cut at {cut}");
            assert_eq!(reopened.state(), durable, "cut at {cut}");
            assert_eq!(fs::metadata(&path).unwrap().len(), durable.total_length);
            assert!(read(&mut reopened, b"torn").is_none());
            assert_eq!(read(&mut reopened, b"durable").unwrap(), b"durable");
        }
    }

    #[test]
    fn unwritten_bytes_of_the_last_frame_end_the_log() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"durable")]);
        let durable = active.state();
        append(&mut active, &[record(b"partly on disk")]);
        let whole = fs::read(&path).unwrap();
        drop(active);
        let frame = durable.total_length as usize;

        // The file grew, but the data of all or part of the frame never landed.
        for zeroed in [
            frame..whole.len(),
            frame + 40..whole.len(),
            frame + 70..frame + 80,
        ] {
            let mut bytes = whole.clone();
            bytes[zeroed.clone()].fill(0);
            fs::write(&path, &bytes).unwrap();
            let (reopened, torn) = recovered(&path);
            assert!(torn, "zeroed {zeroed:?}");
            assert_eq!(reopened.state(), durable, "zeroed {zeroed:?}");
        }
        // Zeros past the last frame are dropped too.
        let mut bytes = whole.clone();
        bytes.extend([0; 100]);
        fs::write(&path, &bytes).unwrap();
        let (reopened, torn) = recovered(&path);
        assert!(torn);
        assert_eq!(reopened.state().total_length, whole.len() as u64);
    }

    #[test]
    fn damage_to_a_frame_with_a_later_frame_is_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"first")]);
        let first_end = active.state().total_length as usize;
        append(&mut active, &[record(b"second")]);
        let whole = fs::read(&path).unwrap();
        drop(active);
        let first_start = HEADER_LEN as usize;

        // A payload byte, an index byte, the trailer, and a header field.
        for at in [
            first_start + FRAME_HEADER_LEN as usize,
            first_end - FRAME_TRAILER_LEN as usize - 1,
            first_end - 1,
            first_start + 10,
        ] {
            let mut bytes = whole.clone();
            bytes[at] ^= 0x55;
            fs::write(&path, &bytes).unwrap();
            assert!(
                matches!(ActiveSegment::recover(&path), Err(ActiveError::Corrupt(_))),
                "flipped byte {at}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "recovery left the file alone"
            );
        }
        let mut bytes = whole;
        bytes[0] ^= 0x55;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            ActiveSegment::recover(&path),
            Err(ActiveError::Corrupt(_))
        ));
    }

    #[test]
    fn an_interrupted_creation_is_unwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        for bytes in [&[][..], &[0; 5][..], &[0; HEADER_LEN as usize][..]] {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                ActiveSegment::recover(&path),
                Err(ActiveError::Unwritten)
            ));
        }
    }

    #[test]
    fn read_only_open_leaves_an_interrupted_append_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"durable")]);
        let durable = active.state();
        drop(active);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"partial frame");
        fs::write(&path, &bytes).unwrap();

        let mut reader = ActiveSegment::open_read_only(&path).unwrap();
        assert_eq!(reader.state(), durable);
        assert_eq!(read(&mut reader, b"durable").unwrap(), b"durable");
        assert_eq!(fs::read(&path).unwrap(), bytes);
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
    fn explicit_count_and_payload_bounds_are_enforced() {
        assert!(matches!(
            validate_projected(MAX_SEGMENT_ENTRIES + 1, 0),
            Err(SegmentError::TooManyEntries)
        ));
        assert!(matches!(
            validate_projected(1, MAX_SEGMENT_PAYLOAD_BYTES + 1),
            Err(SegmentError::TooLarge)
        ));
    }

    #[test]
    fn a_failed_append_truncates_its_frame_and_the_segment_stays_usable() {
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

        append(&mut active, &[record(b"usable afterward")]);
        assert_eq!(active.state().generation, 9);
        assert!(read(&mut active, b"usable afterward").is_some());
        drop(active);
        let (reopened, _) = recovered(&path);
        assert_eq!(reopened.state().generation, 9);
    }

    #[test]
    fn a_failed_sync_truncates_the_frame() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.segment");
        let mut active = ActiveSegment::create(&path, 0).unwrap();
        append(&mut active, &[record(b"published")]);
        let old_state = active.state();
        let prepared = active.prepare_append(&[record(b"unsynced")], 2).unwrap();
        let Ok(write) = active.begin_append(prepared).unwrap() else {
            panic!("a new record is written");
        };
        let mut written = write.run();
        written.test_fail();
        assert!(active.finish_append(written).is_err());
        assert_eq!(active.state(), old_state);
        assert_eq!(fs::metadata(&path).unwrap().len(), old_state.total_length);
        assert!(read(&mut active, b"unsynced").is_none());
    }

    #[test]
    fn sealing_metadata_describes_the_current_log() {
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
                stored_length: state.total_length,
                object_count: 1,
            }
        );
        assert!(!active.should_seal());
    }
}
