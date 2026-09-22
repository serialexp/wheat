//! Immutable, self-authenticating local packed segments.
//!
//! The physical records are the canonical BinSchema records exported by
//! `wheat_core`: prefix, payload bytes, sorted index pages, page directory, and
//! footer. The footer's body digest covers every byte before the footer.

use std::path::Path;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
};

use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::encode_segment_prefix;
use wheat_core::{
    CanonicalObjectError, ContentId, ObjectId, SEGMENT_FOOTER_ENCODED_SIZE,
    SEGMENT_NAMESPACE_CONTENT, SEGMENT_NAMESPACE_METADATA, SEGMENT_PREFIX_ENCODED_SIZE,
    SegmentDirectory, SegmentDirectoryEntry, SegmentFooter, SegmentId, SegmentIndexEntry,
    SegmentIndexKey, SegmentIndexPage, SegmentRecordError, decode_segment_directory,
    decode_segment_footer, decode_segment_index_page, decode_segment_prefix,
    encode_segment_directory, encode_segment_footer, encode_segment_index_page,
    validate_canonical_object,
};

use crate::codec::{self, Codec, PhysicalPayload};

/// Hard decode/allocation ceiling for a V1 segment. Active packs seal in this
/// same format, so readers must accept the active-pack policy bounds.
pub(crate) const MAX_SEGMENT_ENTRIES: usize = 1_000_000;
pub(crate) const MAX_SEGMENT_PAYLOAD_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_SEGMENT_TAIL_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_INDEX_PAGE_ENTRIES: usize = 128;
pub(crate) const METADATA_NAMESPACE: u8 = SEGMENT_NAMESPACE_METADATA;
pub(crate) const CONTENT_NAMESPACE: u8 = SEGMENT_NAMESPACE_CONTENT;

const HASH_BUFFER_SIZE: usize = 64 * 1024;
const PREFIX_LEN: u64 = SEGMENT_PREFIX_ENCODED_SIZE as u64;
const FOOTER_LEN: u64 = SEGMENT_FOOTER_ENCODED_SIZE as u64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentRecord<'a> {
    pub namespace: u8,
    pub kind: u16,
    pub digest: [u8; 32],
    pub bytes: &'a [u8],
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedSegmentRecord<'a> {
    pub logical: SegmentRecord<'a>,
    pub physical: PhysicalPayload,
}

impl PreparedSegmentRecord<'_> {
    pub(crate) fn stored_length(&self) -> u64 {
        self.physical.bytes.len() as u64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SegmentMetadata {
    pub object_count: u64,
    pub payload_bytes: u64,
    pub total_length: u64,
    pub index_start: u64,
    pub directory_offset: u64,
    pub directory_length: u64,
}

#[derive(Debug)]
pub(crate) struct SealedSegment {
    pub file: NamedTempFile,
    pub id: SegmentId,
    pub metadata: SegmentMetadata,
}

#[derive(Debug, Error)]
pub(crate) enum SegmentError {
    #[error("segment I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("segment record failed: {0}")]
    Record(#[from] SegmentRecordError),
    #[error("segment exceeds the {MAX_SEGMENT_ENTRIES} object limit")]
    TooManyEntries,
    #[error("segment exceeds the {MAX_SEGMENT_PAYLOAD_BYTES} byte payload limit")]
    TooLarge,
    #[error("segment record is at least {minimum} bytes, exceeding the read limit")]
    ReadLimit { minimum: usize },
    #[error("duplicate segment key")]
    DuplicateKey,
    #[error("invalid or unsupported segment format: {0}")]
    Format(&'static str),
    #[error("segment body digest does not match its footer")]
    BodyDigest,
    #[error("index page digest does not match its directory entry")]
    PageDigest,
    #[error("stored object digest does not match its index key")]
    ObjectDigest,
    #[error("metadata kind does not match the canonical object")]
    WrongKind,
    #[error("canonical metadata object failed validation: {0}")]
    Canonical(#[from] CanonicalObjectError),
    #[error("segment payload codec failed: {0}")]
    Codec(#[from] codec::CodecError),
}

pub(super) fn key(entry: &SegmentIndexEntry) -> (u8, [u8; 32]) {
    (entry.namespace, entry.digest)
}

pub(crate) fn prepare_record(
    record: SegmentRecord<'_>,
) -> Result<PreparedSegmentRecord<'_>, SegmentError> {
    validate_record(record)?;
    Ok(PreparedSegmentRecord {
        logical: record,
        physical: codec::prepare(record.bytes)?,
    })
}

fn directory_key(entry: &SegmentDirectoryEntry) -> (u8, [u8; 32]) {
    (entry.first_key.namespace, entry.first_key.digest)
}

pub(super) fn validate_record(record: SegmentRecord<'_>) -> Result<(), SegmentError> {
    match record.namespace {
        CONTENT_NAMESPACE if record.kind == 0 => {
            if ContentId::of(record.bytes).as_bytes() != &record.digest {
                return Err(SegmentError::ObjectDigest);
            }
        }
        METADATA_NAMESPACE => {
            let expected = SegmentIndexEntry::canonical_kind(record.kind)
                .ok_or(SegmentError::Format("namespace/kind"))?;
            let actual = validate_canonical_object(record.bytes)?;
            if actual != expected {
                return Err(SegmentError::WrongKind);
            }
            if ObjectId::of_canonical_bytes(record.bytes).as_bytes() != &record.digest {
                return Err(SegmentError::ObjectDigest);
            }
        }
        _ => return Err(SegmentError::Format("namespace/kind")),
    }
    Ok(())
}

/// Exact physical size of a V1 segment with the supplied payload and entry count.
pub(crate) fn projected_total_length(
    entry_count: usize,
    payload_bytes: u64,
) -> Result<u64, SegmentError> {
    if entry_count > MAX_SEGMENT_ENTRIES {
        return Err(SegmentError::TooManyEntries);
    }
    if payload_bytes > MAX_SEGMENT_PAYLOAD_BYTES {
        return Err(SegmentError::TooLarge);
    }
    let count = u64::try_from(entry_count).map_err(|_| SegmentError::TooManyEntries)?;
    let pages = u64::try_from(entry_count.div_ceil(MAX_INDEX_PAGE_ENTRIES))
        .map_err(|_| SegmentError::TooManyEntries)?;
    // Both page and directory records have a four-byte count prefix.
    let page_bytes = count.checked_mul(60).and_then(|n| n.checked_add(pages * 4));
    let directory_bytes = pages.checked_mul(81).and_then(|n| n.checked_add(4));
    let tail_bytes = page_bytes
        .and_then(|pages| directory_bytes.and_then(|directory| pages.checked_add(directory)))
        .and_then(|n| n.checked_add(FOOTER_LEN))
        .ok_or(SegmentError::TooLarge)?;
    if tail_bytes > MAX_SEGMENT_TAIL_BYTES {
        return Err(SegmentError::TooLarge);
    }
    PREFIX_LEN
        .checked_add(payload_bytes)
        .and_then(|n| n.checked_add(tail_bytes))
        .ok_or(SegmentError::TooLarge)
}

/// Target logical bytes per compressed block. Small structural records are
/// buffered until this threshold and then flushed as one zstd frame.
pub(crate) const BLOCK_TARGET_BYTES: usize = 65_536;

/// A pending block record: the validated logical bytes and metadata needed to
/// emit a `SegmentIndexEntry` after the block is compressed.
struct PendingBlockRecord {
    namespace: u8,
    kind: u16,
    digest: [u8; 32],
    logical_bytes: Vec<u8>,
}

/// Streaming builder used to produce immutable V1 segments.
///
/// Small metadata records (below [`codec::RAW_THRESHOLD`]) are buffered and
/// flushed as compressed blocks (codec 4). Larger records are written
/// individually with per-record compression (codec 0 or 1).
pub(crate) struct SegmentBuilder {
    file: NamedTempFile,
    entries: Vec<SegmentIndexEntry>,
    payload_bytes: u64,
    io_buffer: Vec<u8>,
    /// Small records waiting to be compressed into a block.
    pending_block: Vec<PendingBlockRecord>,
    /// Sum of `logical_bytes.len()` across `pending_block`.
    pending_block_bytes: usize,
}

impl SegmentBuilder {
    pub(crate) fn new(directory: impl AsRef<Path>) -> Result<Self, SegmentError> {
        let mut file = NamedTempFile::new_in(directory)?;
        let prefix = encode_segment_prefix()?;
        debug_assert_eq!(prefix.len(), SEGMENT_PREFIX_ENCODED_SIZE);
        file.write_all(&prefix)?;
        Ok(Self {
            file,
            entries: Vec::new(),
            payload_bytes: 0,
            io_buffer: vec![0; HASH_BUFFER_SIZE],
            pending_block: Vec::new(),
            pending_block_bytes: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, record: SegmentRecord<'_>) -> Result<(), SegmentError> {
        self.push_prepared(prepare_record(record)?)
    }

    pub(crate) fn push_prepared(
        &mut self,
        prepared: PreparedSegmentRecord<'_>,
    ) -> Result<(), SegmentError> {
        let record = prepared.logical;

        // Route small metadata records into the pending block buffer.
        // Content namespace records are never block-compressed (they have their
        // own delta/envelope compression).
        if record.namespace == METADATA_NAMESPACE && record.bytes.len() < codec::RAW_THRESHOLD {
            self.pending_block.push(PendingBlockRecord {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                logical_bytes: record.bytes.to_vec(),
            });
            self.pending_block_bytes += record.bytes.len();
            if self.pending_block_bytes >= BLOCK_TARGET_BYTES {
                self.flush_pending_block()?;
            }
            return Ok(());
        }

        // Large record: write individually with per-record compression.
        if self.entries.len() == MAX_SEGMENT_ENTRIES {
            return Err(SegmentError::TooManyEntries);
        }
        let physical = prepared.physical;
        let stored_length = physical.bytes.len() as u64;
        if self
            .payload_bytes
            .checked_add(stored_length)
            .filter(|n| *n <= MAX_SEGMENT_PAYLOAD_BYTES)
            .is_none()
        {
            return Err(SegmentError::TooLarge);
        }
        let offset = PREFIX_LEN + self.payload_bytes;
        self.file.write_all(&physical.bytes)?;
        self.payload_bytes += stored_length;
        self.entries.push(SegmentIndexEntry {
            namespace: record.namespace,
            kind: record.kind,
            digest: record.digest,
            offset,
            codec: physical.codec.to_byte(),
            stored_length,
            logical_length: physical.logical_length,
        });
        Ok(())
    }

    /// Compresses the pending block records into one zstd frame and writes it.
    fn flush_pending_block(&mut self) -> Result<(), SegmentError> {
        if self.pending_block.is_empty() {
            return Ok(());
        }

        // Sort records by (namespace, digest) — the same deterministic order
        // the reader uses to reconstruct within-block positions.
        self.pending_block
            .sort_unstable_by_key(|r| (r.namespace, r.digest));

        // Concatenate logical bytes.
        let total_logical: usize = self.pending_block_bytes;
        let mut concatenated = Vec::with_capacity(total_logical);
        for record in &self.pending_block {
            concatenated.extend_from_slice(&record.logical_bytes);
        }

        // Compress as one zstd frame.
        let compressed = zstd::bulk::compress(&concatenated, codec::ZSTD_LEVEL)
            .map_err(codec::CodecError::Zstd)?;

        // Check limits.
        let entry_count = self.pending_block.len();
        if self.entries.len() + entry_count > MAX_SEGMENT_ENTRIES {
            return Err(SegmentError::TooManyEntries);
        }
        let stored_length = compressed.len() as u64;
        if self
            .payload_bytes
            .checked_add(stored_length)
            .filter(|n| *n <= MAX_SEGMENT_PAYLOAD_BYTES)
            .is_none()
        {
            return Err(SegmentError::TooLarge);
        }

        // Write compressed block.
        let block_offset = PREFIX_LEN + self.payload_bytes;
        self.file.write_all(&compressed)?;
        self.payload_bytes += stored_length;

        // Emit one SegmentIndexEntry per record. All share the same
        // (offset, stored_length); each has its own logical_length.
        for record in self.pending_block.drain(..) {
            self.entries.push(SegmentIndexEntry {
                namespace: record.namespace,
                kind: record.kind,
                digest: record.digest,
                offset: block_offset,
                codec: Codec::ZstdBlock.to_byte(),
                stored_length,
                logical_length: record.logical_bytes.len() as u64,
            });
        }
        self.pending_block_bytes = 0;
        Ok(())
    }

    pub(crate) fn seal(mut self) -> Result<SealedSegment, SegmentError> {
        // Flush any remaining pending block records.
        self.flush_pending_block()?;

        let index_start = PREFIX_LEN
            .checked_add(self.payload_bytes)
            .ok_or(SegmentError::TooLarge)?;
        let footer = emit_cumulative_tail(
            self.file.as_file_mut(),
            &mut self.entries,
            index_start,
            &mut self.io_buffer,
        )?;
        self.file.flush()?;

        Ok(SealedSegment {
            file: self.file,
            id: footer.segment_id(),
            metadata: metadata(&footer),
        })
    }
}

/// Replaces the V1 cumulative index/directory/footer at `index_start`.
///
/// Payload bytes before `index_start` are never rewritten. `entries` is sorted
/// in place and the supplied I/O buffer is reused while hashing the complete
/// body. On success the file length is exactly the returned footer length.
pub(crate) fn emit_cumulative_tail(
    file: &mut File,
    entries: &mut [SegmentIndexEntry],
    index_start: u64,
    io_buffer: &mut Vec<u8>,
) -> Result<SegmentFooter, SegmentError> {
    if entries.len() > MAX_SEGMENT_ENTRIES {
        return Err(SegmentError::TooManyEntries);
    }
    let payload_bytes = index_start
        .checked_sub(PREFIX_LEN)
        .ok_or(SegmentError::Format("index start"))?;
    if payload_bytes > MAX_SEGMENT_PAYLOAD_BYTES {
        return Err(SegmentError::TooLarge);
    }
    entries.sort_unstable_by_key(key);
    if entries.windows(2).any(|w| key(&w[0]) == key(&w[1])) {
        return Err(SegmentError::DuplicateKey);
    }

    file.set_len(index_start)?;
    file.seek(SeekFrom::Start(index_start))?;
    let page_count = entries.len().div_ceil(MAX_INDEX_PAGE_ENTRIES);
    let mut pages = Vec::with_capacity(page_count);
    let mut page = SegmentIndexPage {
        entries: Vec::with_capacity(MAX_INDEX_PAGE_ENTRIES),
    };
    for chunk in entries.chunks(MAX_INDEX_PAGE_ENTRIES) {
        page.entries.clear();
        page.entries.extend_from_slice(chunk);
        let offset = file.stream_position()?;
        let encoded = encode_segment_index_page(&page)?;
        let length = u64::try_from(encoded.len()).map_err(|_| SegmentError::TooLarge)?;
        file.write_all(&encoded)?;
        pages.push(SegmentDirectoryEntry {
            first_key: SegmentIndexKey {
                namespace: chunk[0].namespace,
                digest: chunk[0].digest,
            },
            offset,
            length,
            page_digest: *blake3::hash(&encoded).as_bytes(),
        });
    }

    let directory_offset = file.stream_position()?;
    let encoded = encode_segment_directory(&SegmentDirectory { pages })?;
    let directory_length = u64::try_from(encoded.len()).map_err(|_| SegmentError::TooLarge)?;
    file.write_all(&encoded)?;
    let body_length = directory_offset
        .checked_add(directory_length)
        .ok_or(SegmentError::TooLarge)?;
    let tail_length = body_length
        .checked_sub(index_start)
        .and_then(|length| length.checked_add(FOOTER_LEN))
        .ok_or(SegmentError::TooLarge)?;
    if tail_length > MAX_SEGMENT_TAIL_BYTES {
        return Err(SegmentError::TooLarge);
    }

    if io_buffer.len() != HASH_BUFFER_SIZE {
        io_buffer.resize(HASH_BUFFER_SIZE, 0);
    }
    let body_digest = hash_range_buffered(file, 0, body_length, io_buffer)?;
    let total_length = body_length
        .checked_add(FOOTER_LEN)
        .ok_or(SegmentError::TooLarge)?;
    let footer = SegmentFooter {
        total_length,
        object_count: entries.len() as u64,
        index_start,
        directory_offset,
        directory_length,
        body_digest,
    };
    let encoded = encode_segment_footer(&footer)?;
    debug_assert_eq!(encoded.len(), SEGMENT_FOOTER_ENCODED_SIZE);
    file.seek(SeekFrom::Start(body_length))?;
    file.write_all(&encoded)?;
    file.set_len(total_length)?;
    Ok(footer)
}

pub(crate) struct SegmentReader {
    file: File,
    id: SegmentId,
    metadata: SegmentMetadata,
    entries_by_key: Vec<SegmentIndexEntry>,
    /// For codec=4 entries: byte offset of this record within the decompressed
    /// block. Zero for non-block entries. Indexed in parallel with
    /// `entries_by_key`.
    block_logical_offsets: Vec<u64>,
    /// Cache of decompressed block payloads, keyed by the block's physical
    /// byte offset in the segment file.
    block_cache: std::collections::HashMap<u64, Vec<u8>>,
}

impl SegmentReader {
    /// Opens and fully authenticates the body once. Payloads are not retained.
    pub(crate) fn open(file: File) -> Result<Self, SegmentError> {
        let actual_len = file.metadata()?.len();
        Self::open_at_length(file, actual_len)
    }

    /// Authenticates a published prefix while ignoring an uncommitted suffix.
    pub(super) fn open_at_published_length(
        file: File,
        published_length: u64,
    ) -> Result<Self, SegmentError> {
        if file.metadata()?.len() < published_length {
            return Err(SegmentError::Format("truncated published segment"));
        }
        Self::open_at_length(file, published_length)
    }

    fn open_at_length(mut file: File, actual_len: u64) -> Result<Self, SegmentError> {
        // Bound all subsequent allocations, including a forged directory length.
        const MAX_PHYSICAL_BYTES: u64 =
            MAX_SEGMENT_PAYLOAD_BYTES + MAX_SEGMENT_TAIL_BYTES + FOOTER_LEN;
        if !(PREFIX_LEN + FOOTER_LEN..=MAX_PHYSICAL_BYTES).contains(&actual_len) {
            return Err(SegmentError::Format("truncated or oversized"));
        }
        decode_segment_prefix(&read_range(&mut file, 0, PREFIX_LEN)?)?;

        let footer_offset = actual_len - FOOTER_LEN;
        let footer = decode_segment_footer(&read_range(&mut file, footer_offset, FOOTER_LEN)?)?;
        if footer.total_length != actual_len
            || footer.object_count > MAX_SEGMENT_ENTRIES as u64
            || footer.index_start < PREFIX_LEN
            || footer.index_start - PREFIX_LEN > MAX_SEGMENT_PAYLOAD_BYTES
            || footer.directory_offset < footer.index_start
            || footer
                .index_start
                .checked_add(MAX_SEGMENT_TAIL_BYTES)
                .is_none_or(|tail_end| actual_len > tail_end)
            || footer.directory_offset.checked_add(footer.directory_length) != Some(footer_offset)
        {
            return Err(SegmentError::Format("footer offsets"));
        }
        if hash_range(&mut file, 0, footer_offset)? != footer.body_digest {
            return Err(SegmentError::BodyDigest);
        }

        let raw_directory =
            read_range(&mut file, footer.directory_offset, footer.directory_length)?;
        let directory = decode_segment_directory(&raw_directory)?.pages;
        if directory
            .windows(2)
            .any(|w| directory_key(&w[0]) >= directory_key(&w[1]))
        {
            return Err(SegmentError::Format("directory ordering"));
        }

        let mut all_entries = Vec::new();
        let mut next_page_offset = footer.index_start;
        let mut previous = None;
        for descriptor in &directory {
            if descriptor.offset != next_page_offset
                || descriptor
                    .offset
                    .checked_add(descriptor.length)
                    .is_none_or(|end| end > footer.directory_offset)
            {
                return Err(SegmentError::Format("page bounds"));
            }
            next_page_offset = descriptor.offset + descriptor.length;
            let page = read_range(&mut file, descriptor.offset, descriptor.length)?;
            if blake3::hash(&page).as_bytes() != &descriptor.page_digest {
                return Err(SegmentError::PageDigest);
            }
            let entries = decode_segment_index_page(&page)?.entries;
            if entries.is_empty()
                || entries.len() > MAX_INDEX_PAGE_ENTRIES
                || entries.first().map(key) != Some(directory_key(descriptor))
            {
                return Err(SegmentError::Format("directory first key"));
            }
            for entry in &entries {
                if previous.is_some_and(|old| old >= key(entry)) {
                    return Err(SegmentError::Format("index ordering"));
                }
                previous = Some(key(entry));
            }
            if all_entries.len() + entries.len() > MAX_SEGMENT_ENTRIES {
                return Err(SegmentError::TooManyEntries);
            }
            all_entries.extend(entries);
        }
        if next_page_offset != footer.directory_offset
            || all_entries.len() as u64 != footer.object_count
            || (footer.object_count == 0) != directory.is_empty()
        {
            return Err(SegmentError::Format("object count or index coverage"));
        }

        // Payload ranges must cover precisely the payload body. For codec=4
        // (ZstdBlock) entries, multiple entries share the same (offset,
        // stored_length) — they belong to the same compressed block.
        // Deduplicate by (offset, stored_length) to get the unique payload
        // regions, then check contiguous coverage.
        all_entries.sort_unstable_by_key(|entry| (entry.offset, entry.stored_length));
        {
            let mut next_payload_offset = PREFIX_LEN;
            let mut prev_region: Option<(u64, u64)> = None;
            for entry in &all_entries {
                if entry.logical_length > codec::HARD_LOGICAL_RECORD_BYTES as u64 {
                    return Err(SegmentError::Format("payload offsets or logical length"));
                }
                let region = (entry.offset, entry.stored_length);
                if prev_region == Some(region) {
                    // Same block as the previous entry — already accounted for.
                    continue;
                }
                if entry.offset != next_payload_offset
                    || entry
                        .offset
                        .checked_add(entry.stored_length)
                        .is_none_or(|end| end > footer.index_start)
                {
                    return Err(SegmentError::Format("payload offsets or logical length"));
                }
                next_payload_offset = entry.offset + entry.stored_length;
                prev_region = Some(region);
            }
            if next_payload_offset != footer.index_start {
                return Err(SegmentError::Format("payload coverage"));
            }
        }

        // Re-sort by key for binary-search lookup.
        all_entries.sort_unstable_by_key(key);

        // Precompute within-block byte offsets for codec=4 entries.
        let block_logical_offsets = compute_block_logical_offsets(&all_entries);

        Ok(Self {
            file,
            id: footer.segment_id(),
            metadata: metadata(&footer),
            entries_by_key: all_entries,
            block_logical_offsets,
            block_cache: std::collections::HashMap::new(),
        })
    }

    pub(crate) fn id(&self) -> SegmentId {
        self.id
    }

    pub(crate) fn metadata(&self) -> SegmentMetadata {
        self.metadata
    }

    pub(crate) fn into_authenticated_parts(
        self,
    ) -> (File, SegmentId, SegmentMetadata, Vec<SegmentIndexEntry>) {
        (self.file, self.id, self.metadata, self.entries_by_key)
    }

    /// Borrows the index decoded and authenticated by `open`.
    pub(crate) fn authenticated_entries(&self) -> &[SegmentIndexEntry] {
        &self.entries_by_key
    }

    /// Returns the authenticated index entry and its logically verified payload.
    /// The index is neither read nor decoded again, and the payload length is
    /// checked against `max` before allocating its output buffer.
    pub(crate) fn lookup_entry_bounded(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
        max: usize,
    ) -> Result<Option<(&SegmentIndexEntry, Vec<u8>)>, SegmentError> {
        let wanted = (namespace, digest);
        let Ok(index) = self.entries_by_key.binary_search_by_key(&wanted, key) else {
            return Ok(None);
        };
        let entry = &self.entries_by_key[index];
        let logical_length =
            usize::try_from(entry.logical_length).map_err(|_| SegmentError::ReadLimit {
                minimum: usize::MAX,
            })?;
        if logical_length > max {
            return Err(SegmentError::ReadLimit {
                minimum: logical_length,
            });
        }
        let codec = Codec::from_byte(entry.codec)?;
        let bytes =
            if codec == Codec::ZstdBlock {
                let block_offset = entry.offset;
                let block_stored_length = entry.stored_length;
                let within_block_offset = usize::try_from(self.block_logical_offsets[index])
                    .map_err(|_| SegmentError::ReadLimit {
                        minimum: usize::MAX,
                    })?;

                // Decompress and cache the block.
                if !self.block_cache.contains_key(&block_offset) {
                    let compressed = read_range(&mut self.file, block_offset, block_stored_length)?;
                    let decompressed = codec::decode_block(&compressed)?;
                    self.block_cache.insert(block_offset, decompressed);
                }
                let block = &self.block_cache[&block_offset];
                let end = within_block_offset
                    .checked_add(logical_length)
                    .ok_or(SegmentError::Format("block record overflows"))?;
                if end > block.len() {
                    return Err(SegmentError::Format(
                        "block record exceeds decompressed block",
                    ));
                }
                block[within_block_offset..end].to_vec()
            } else {
                let stored = read_range(&mut self.file, entry.offset, entry.stored_length)?;
                codec::decode(codec, stored, entry.logical_length, max)?
            };
        verify_logical(entry, &bytes)?;
        Ok(Some((entry, bytes)))
    }

    #[cfg(test)]
    pub(crate) fn lookup(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, SegmentError> {
        Ok(self
            .lookup_entry_bounded(namespace, digest, usize::MAX)?
            .map(|(entry, bytes)| (entry.kind, bytes)))
    }
}

pub(super) fn metadata(footer: &SegmentFooter) -> SegmentMetadata {
    SegmentMetadata {
        object_count: footer.object_count,
        payload_bytes: footer.index_start - PREFIX_LEN,
        total_length: footer.total_length,
        index_start: footer.index_start,
        directory_offset: footer.directory_offset,
        directory_length: footer.directory_length,
    }
}

/// Computes the logical byte offset of each record within its compressed block.
///
/// `entries` must already be sorted by key (namespace, digest). Records with
/// codec=4 that share the same `offset` value belong to the same block. Their
/// logical bytes are concatenated in key-sorted order within the decompressed
/// block, so the offset of record i is the sum of `logical_length` of all
/// preceding records in the same block.
///
/// Non-block entries get offset 0 (unused).
fn compute_block_logical_offsets(entries: &[SegmentIndexEntry]) -> Vec<u64> {
    use std::collections::HashMap;
    let mut offsets = vec![0u64; entries.len()];
    // Group block entries by block physical offset, preserving key-sorted order.
    let mut blocks: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.codec == Codec::ZstdBlock.to_byte() {
            blocks.entry(entry.offset).or_default().push(i);
        }
    }
    for indices in blocks.values() {
        // Indices are already in key order because entries is key-sorted and we
        // iterate sequentially.
        let mut cumulative = 0u64;
        for &i in indices {
            offsets[i] = cumulative;
            cumulative += entries[i].logical_length;
        }
    }
    offsets
}

pub(super) fn verify_logical(entry: &SegmentIndexEntry, bytes: &[u8]) -> Result<(), SegmentError> {
    match entry.namespace {
        CONTENT_NAMESPACE => {
            if ContentId::of(bytes).as_bytes() != &entry.digest {
                return Err(SegmentError::ObjectDigest);
            }
        }
        METADATA_NAMESPACE => {
            if ObjectId::of_canonical_bytes(bytes).as_bytes() != &entry.digest {
                return Err(SegmentError::ObjectDigest);
            }
            let expected =
                SegmentIndexEntry::canonical_kind(entry.kind).ok_or(SegmentError::WrongKind)?;
            if validate_canonical_object(bytes)? != expected {
                return Err(SegmentError::WrongKind);
            }
        }
        _ => return Err(SegmentError::Format("namespace")),
    }
    Ok(())
}

fn hash_range(file: &mut File, offset: u64, length: u64) -> Result<[u8; 32], SegmentError> {
    let mut buffer = vec![0; HASH_BUFFER_SIZE];
    hash_range_buffered(file, offset, length, &mut buffer)
}

fn hash_range_buffered(
    file: &mut File,
    offset: u64,
    length: u64,
    buffer: &mut [u8],
) -> Result<[u8; 32], SegmentError> {
    if buffer.is_empty() {
        return Err(SegmentError::Format("empty I/O buffer"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut hasher = blake3::Hasher::new();
    while remaining != 0 {
        let amount = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..amount])?;
        hasher.update(&buffer[..amount]);
        remaining -= amount as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

fn read_range(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>, SegmentError> {
    let size = usize::try_from(length).map_err(|_| SegmentError::Format("range too large"))?;
    let mut bytes = vec![0; size];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_record(bytes: &[u8]) -> SegmentRecord<'_> {
        SegmentRecord {
            namespace: CONTENT_NAMESPACE,
            kind: 0,
            digest: *ContentId::of(bytes).as_bytes(),
            bytes,
        }
    }

    fn build(values: &[&[u8]]) -> SealedSegment {
        let mut builder = SegmentBuilder::new(std::env::temp_dir()).unwrap();
        for value in values {
            builder.push(content_record(value)).unwrap();
        }
        builder.seal().unwrap()
    }

    fn bytes(segment: &SealedSegment) -> Vec<u8> {
        let mut file = segment.file.reopen().unwrap();
        let mut result = Vec::new();
        file.read_to_end(&mut result).unwrap();
        result
    }

    fn file_with_bytes(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    fn footer(bytes: &[u8]) -> SegmentFooter {
        decode_segment_footer(&bytes[bytes.len() - SEGMENT_FOOTER_ENCODED_SIZE..]).unwrap()
    }

    fn repair_body_digest(bytes: &mut [u8]) {
        let footer_at = bytes.len() - SEGMENT_FOOTER_ENCODED_SIZE;
        let mut value = decode_segment_footer(&bytes[footer_at..]).unwrap();
        value.body_digest = *blake3::hash(&bytes[..footer_at]).as_bytes();
        bytes[footer_at..].copy_from_slice(&encode_segment_footer(&value).unwrap());
    }

    #[test]
    fn canonical_records_namespaces_readback_and_exact_coverage() {
        let first = b"first payload".as_slice();
        let second = b"second payload".as_slice();
        let segment = build(&[first, second]);
        let raw = bytes(&segment);
        let decoded_footer = footer(&raw);

        decode_segment_prefix(&raw[..SEGMENT_PREFIX_ENCODED_SIZE]).unwrap();
        assert_eq!(segment.id, decoded_footer.segment_id());
        assert_eq!(segment.metadata, metadata(&decoded_footer));
        assert_eq!(
            decoded_footer.index_start,
            PREFIX_LEN + first.len() as u64 + second.len() as u64
        );
        assert_eq!(
            decoded_footer.directory_offset + decoded_footer.directory_length,
            raw.len() as u64 - FOOTER_LEN
        );
        assert_eq!(
            decoded_footer.body_digest,
            *blake3::hash(&raw[..raw.len() - SEGMENT_FOOTER_ENCODED_SIZE]).as_bytes()
        );

        let directory = decode_segment_directory(
            &raw[decoded_footer.directory_offset as usize
                ..(decoded_footer.directory_offset + decoded_footer.directory_length) as usize],
        )
        .unwrap();
        assert_eq!(directory.pages.len(), 1);
        let descriptor = &directory.pages[0];
        let page = decode_segment_index_page(
            &raw[descriptor.offset as usize..(descriptor.offset + descriptor.length) as usize],
        )
        .unwrap();
        assert!(
            page.entries
                .iter()
                .all(|entry| { entry.namespace == SEGMENT_NAMESPACE_CONTENT && entry.kind == 0 })
        );

        let mut reader = SegmentReader::open(segment.file.reopen().unwrap()).unwrap();
        assert_eq!(reader.id(), segment.id);
        assert_eq!(reader.metadata(), segment.metadata);
        assert_eq!(
            reader
                .lookup(CONTENT_NAMESPACE, *ContentId::of(second).as_bytes())
                .unwrap(),
            Some((0, second.to_vec()))
        );
        assert_eq!(reader.lookup(METADATA_NAMESPACE, [7; 32]).unwrap(), None);
    }

    #[test]
    fn zero_length_payload_before_a_nonempty_payload_authenticates() {
        let empty = b"".as_slice();
        let nonempty = b"payload".as_slice();
        let segment = build(&[empty, nonempty]);
        let mut reader = SegmentReader::open(segment.file.reopen().unwrap()).unwrap();
        assert_eq!(
            reader
                .lookup(CONTENT_NAMESPACE, *ContentId::of(empty).as_bytes())
                .unwrap(),
            Some((0, Vec::new()))
        );
        assert_eq!(
            reader
                .lookup(CONTENT_NAMESPACE, *ContentId::of(nonempty).as_bytes())
                .unwrap(),
            Some((0, nonempty.to_vec()))
        );
    }

    #[test]
    fn edit_metadata_packs_and_reads_back() {
        let edit = wheat_core::EditRecord::new(
            "edited.txt",
            wheat_core::EditOperation::ExactFile {
                expected: None,
                replacement: Some(wheat_core::FileValue {
                    content: ContentId::of(b"replacement"),
                    kind: wheat_core::FileKind::Regular,
                }),
            },
        )
        .unwrap();
        let canonical = edit.canonical_bytes();
        let digest = *ObjectId::of_canonical_bytes(&canonical).as_bytes();
        let kind = SegmentIndexEntry::canonical_kind_code(wheat_core::CanonicalObjectKind::Edit);
        assert_eq!(kind, 16);

        let directory = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(directory.path()).unwrap();
        builder
            .push(SegmentRecord {
                namespace: METADATA_NAMESPACE,
                kind,
                digest,
                bytes: &canonical,
            })
            .unwrap();
        let segment = builder.seal().unwrap();
        let mut reader = SegmentReader::open(segment.file.reopen().unwrap()).unwrap();
        assert_eq!(
            reader.lookup(METADATA_NAMESPACE, digest).unwrap(),
            Some((kind, canonical.clone()))
        );
        let wrong_known_kind =
            SegmentIndexEntry::canonical_kind_code(wheat_core::CanonicalObjectKind::Tree);
        assert!(matches!(
            verify_logical(
                &SegmentIndexEntry {
                    namespace: METADATA_NAMESPACE,
                    kind: wrong_known_kind,
                    digest,
                    offset: 0,
                    codec: 0,
                    stored_length: canonical.len() as u64,
                    logical_length: canonical.len() as u64,
                },
                &canonical,
            ),
            Err(SegmentError::WrongKind)
        ));
    }

    #[test]
    fn rejects_invalid_namespace_kinds_and_truncation() {
        let directory = tempfile::tempdir().unwrap();
        for (namespace, kind) in [
            (0, 0),
            (CONTENT_NAMESPACE, 1),
            (METADATA_NAMESPACE, 0),
            (METADATA_NAMESPACE, 21),
        ] {
            let mut builder = SegmentBuilder::new(directory.path()).unwrap();
            assert!(matches!(
                builder.push(SegmentRecord {
                    namespace,
                    kind,
                    digest: [0; 32],
                    bytes: b"x"
                }),
                Err(SegmentError::Format("namespace/kind"))
            ));
        }

        let segment = build(&[b"payload"]);
        let mut raw = bytes(&segment);
        raw.pop();
        assert!(SegmentReader::open(file_with_bytes(&raw)).is_err());
    }

    #[test]
    fn rejects_corrupt_body_and_page_digest() {
        let segment = build(&[b"payload"]);
        let mut raw = bytes(&segment);
        raw[SEGMENT_PREFIX_ENCODED_SIZE] ^= 1;
        assert!(matches!(
            SegmentReader::open(file_with_bytes(&raw)),
            Err(SegmentError::BodyDigest)
        ));

        let mut raw = bytes(&segment);
        let value = footer(&raw);
        raw[value.index_start as usize + 4] ^= 1;
        repair_body_digest(&mut raw);
        assert!(matches!(
            SegmentReader::open(file_with_bytes(&raw)),
            Err(SegmentError::PageDigest)
        ));
    }

    fn replace_page(raw: &mut [u8], mutate: impl FnOnce(&mut SegmentIndexPage)) {
        let value = footer(raw);
        let directory_range = value.directory_offset as usize
            ..(value.directory_offset + value.directory_length) as usize;
        let mut directory = decode_segment_directory(&raw[directory_range.clone()]).unwrap();
        let descriptor = &mut directory.pages[0];
        let page_range =
            descriptor.offset as usize..(descriptor.offset + descriptor.length) as usize;
        let mut page = decode_segment_index_page(&raw[page_range.clone()]).unwrap();
        mutate(&mut page);
        let encoded = encode_segment_index_page(&page).unwrap();
        assert_eq!(encoded.len(), page_range.len());
        raw[page_range].copy_from_slice(&encoded);
        descriptor.page_digest = *blake3::hash(&encoded).as_bytes();
        let encoded_directory = encode_segment_directory(&directory).unwrap();
        assert_eq!(encoded_directory.len(), directory_range.len());
        raw[directory_range].copy_from_slice(&encoded_directory);
        repair_body_digest(raw);
    }

    #[test]
    fn rejects_page_ordering_and_non_covering_payload_offsets() {
        let segment = build(&[b"a", b"b"]);
        let mut raw = bytes(&segment);
        replace_page(&mut raw, |page| page.entries.swap(0, 1));
        assert!(matches!(
            SegmentReader::open(file_with_bytes(&raw)),
            Err(SegmentError::Format("directory first key"))
                | Err(SegmentError::Format("index ordering"))
        ));

        let mut raw = bytes(&segment);
        replace_page(&mut raw, |page| page.entries[0].offset += 1);
        assert!(matches!(
            SegmentReader::open(file_with_bytes(&raw)),
            Err(SegmentError::Format("payload offsets or logical length"))
        ));
    }

    /// Creates a small Edit metadata record (always <1024 bytes, so
    /// SegmentBuilder will route it to block compression).
    fn make_edit(path: &str) -> (u16, [u8; 32], Vec<u8>) {
        let edit = wheat_core::EditRecord::new(
            path,
            wheat_core::EditOperation::ExactFile {
                expected: None,
                replacement: Some(wheat_core::FileValue {
                    content: ContentId::of(path.as_bytes()),
                    kind: wheat_core::FileKind::Regular,
                }),
            },
        )
        .unwrap();
        let canonical = edit.canonical_bytes();
        let digest = *ObjectId::of_canonical_bytes(&canonical).as_bytes();
        let kind = SegmentIndexEntry::canonical_kind_code(wheat_core::CanonicalObjectKind::Edit);
        (kind, digest, canonical)
    }

    #[test]
    fn block_compressed_metadata_records_read_back_correctly() {
        // Push enough small metadata records to trigger block compression.
        let directory = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(directory.path()).unwrap();
        let mut expected: Vec<(u16, [u8; 32], Vec<u8>)> = Vec::new();
        for i in 0..20 {
            let (kind, digest, canonical) = make_edit(&format!("file_{i:03}.txt"));
            builder
                .push(SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind,
                    digest,
                    bytes: &canonical,
                })
                .unwrap();
            expected.push((kind, digest, canonical));
        }
        let segment = builder.seal().unwrap();
        let mut reader = SegmentReader::open(segment.file.reopen().unwrap()).unwrap();

        // Verify every record reads back correctly.
        for (kind, digest, canonical) in &expected {
            let (got_kind, got_bytes) =
                reader.lookup(METADATA_NAMESPACE, *digest).unwrap().unwrap();
            assert_eq!(got_kind, *kind);
            assert_eq!(got_bytes, *canonical);
        }

        // Verify that block compression was actually used: at least one entry
        // has codec=4.
        let has_block = reader
            .authenticated_entries()
            .iter()
            .any(|e| e.codec == Codec::ZstdBlock.to_byte());
        assert!(
            has_block,
            "expected at least one block-compressed entry, but all entries \
             have codec {:?}",
            reader
                .authenticated_entries()
                .iter()
                .map(|e| e.codec)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn mixed_block_and_standalone_records_in_same_segment() {
        // Mix small metadata records (block-compressed) with content records
        // (always standalone).
        let directory = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(directory.path()).unwrap();

        // 10 small metadata records.
        let mut edits = Vec::new();
        for i in 0..10 {
            let (kind, digest, canonical) = make_edit(&format!("mixed_{i}.rs"));
            builder
                .push(SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind,
                    digest,
                    bytes: &canonical,
                })
                .unwrap();
            edits.push((digest, canonical));
        }

        // 3 content records (standalone, not block-compressed).
        let contents: Vec<Vec<u8>> = (0..3)
            .map(|i| format!("content-{i}").into_bytes())
            .collect();
        for content in &contents {
            builder.push(content_record(content)).unwrap();
        }

        let segment = builder.seal().unwrap();
        let mut reader = SegmentReader::open(segment.file.reopen().unwrap()).unwrap();

        // Verify metadata records.
        for (digest, canonical) in &edits {
            let (_, got_bytes) = reader.lookup(METADATA_NAMESPACE, *digest).unwrap().unwrap();
            assert_eq!(got_bytes, *canonical);
        }

        // Verify content records.
        for content in &contents {
            let (kind, got_bytes) = reader
                .lookup(CONTENT_NAMESPACE, *ContentId::of(content).as_bytes())
                .unwrap()
                .unwrap();
            assert_eq!(kind, 0);
            assert_eq!(got_bytes, *content);
        }

        // Verify mix of codecs: metadata should be block-compressed, content raw.
        let entries = reader.authenticated_entries();
        let metadata_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.namespace == METADATA_NAMESPACE)
            .collect();
        let content_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.namespace == CONTENT_NAMESPACE)
            .collect();
        assert!(
            metadata_entries
                .iter()
                .all(|e| e.codec == Codec::ZstdBlock.to_byte())
        );
        assert!(
            content_entries
                .iter()
                .all(|e| e.codec == Codec::Raw.to_byte())
        );
    }

    #[test]
    fn block_compression_reduces_payload_bytes() {
        // Build the same records with and without block compression by
        // controlling the record size relative to RAW_THRESHOLD.
        let directory = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(directory.path()).unwrap();
        for i in 0..20 {
            let (kind, digest, canonical) = make_edit(&format!("compress_{i:03}.txt"));
            builder
                .push(SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind,
                    digest,
                    bytes: &canonical,
                })
                .unwrap();
        }
        let segment = builder.seal().unwrap();
        let compressed_payload = segment.metadata.payload_bytes;
        let raw_total: u64 = reader_payload_logical_total(segment.file.reopen().unwrap());

        // Block compression should reduce payload bytes relative to the sum
        // of all logical record sizes.
        assert!(
            compressed_payload < raw_total,
            "block-compressed payload ({compressed_payload}) should be less \
             than raw total ({raw_total})"
        );
    }

    fn reader_payload_logical_total(file: File) -> u64 {
        let reader = SegmentReader::open(file).unwrap();
        reader
            .authenticated_entries()
            .iter()
            .map(|e| e.logical_length)
            .sum()
    }
}
