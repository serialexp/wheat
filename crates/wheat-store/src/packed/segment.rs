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
}

pub(super) fn key(entry: &SegmentIndexEntry) -> (u8, [u8; 32]) {
    (entry.namespace, entry.digest)
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
    let page_bytes = count.checked_mul(51).and_then(|n| n.checked_add(pages * 4));
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

/// Streaming builder used to produce immutable V1 segments.
pub(crate) struct SegmentBuilder {
    file: NamedTempFile,
    entries: Vec<SegmentIndexEntry>,
    payload_bytes: u64,
    io_buffer: Vec<u8>,
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
        })
    }

    pub(crate) fn push(&mut self, record: SegmentRecord<'_>) -> Result<(), SegmentError> {
        if self.entries.len() == MAX_SEGMENT_ENTRIES {
            return Err(SegmentError::TooManyEntries);
        }
        validate_record(record)?;
        let length = u64::try_from(record.bytes.len()).map_err(|_| SegmentError::TooLarge)?;
        if self
            .payload_bytes
            .checked_add(length)
            .filter(|n| *n <= MAX_SEGMENT_PAYLOAD_BYTES)
            .is_none()
        {
            return Err(SegmentError::TooLarge);
        }
        let offset = PREFIX_LEN + self.payload_bytes;
        self.file.write_all(record.bytes)?;
        self.payload_bytes += length;
        self.entries.push(SegmentIndexEntry {
            namespace: record.namespace,
            kind: record.kind,
            digest: record.digest,
            offset,
            length,
        });
        Ok(())
    }

    pub(crate) fn seal(mut self) -> Result<SealedSegment, SegmentError> {
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

        // Payload ranges must cover precisely the payload body, regardless of key order.
        // Empty payloads can share an offset with the following non-empty payload;
        // order zero-length entries first so key sorting cannot make valid coverage
        // appear overlapping.
        all_entries.sort_unstable_by_key(|entry| (entry.offset, entry.length));
        let mut next_payload_offset = PREFIX_LEN;
        for entry in &all_entries {
            if entry.offset != next_payload_offset
                || entry
                    .offset
                    .checked_add(entry.length)
                    .is_none_or(|end| end > footer.index_start)
            {
                return Err(SegmentError::Format("payload offsets"));
            }
            next_payload_offset = entry.offset + entry.length;
        }
        if next_payload_offset != footer.index_start {
            return Err(SegmentError::Format("payload coverage"));
        }

        all_entries.sort_unstable_by_key(key);
        Ok(Self {
            file,
            id: footer.segment_id(),
            metadata: metadata(&footer),
            entries_by_key: all_entries,
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
        let length = usize::try_from(entry.length).map_err(|_| SegmentError::ReadLimit {
            minimum: usize::MAX,
        })?;
        if length > max {
            return Err(SegmentError::ReadLimit { minimum: length });
        }
        let bytes = read_range(&mut self.file, entry.offset, entry.length)?;
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
                    length: canonical.len() as u64,
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
            (METADATA_NAMESPACE, 19),
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
            Err(SegmentError::Format("payload offsets"))
        ));
    }
}
