//! Immutable, self-authenticating local packed segments.
//!
//! The physical records are the canonical BinSchema records exported by
//! `wheat_core`: prefix, payload bytes, sorted index pages, page directory, and
//! footer. The footer's body digest covers every byte before the footer.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    CanonicalObjectKind, ContentId, ObjectId, SEGMENT_FOOTER_ENCODED_SIZE,
    SEGMENT_NAMESPACE_CONTENT, SEGMENT_NAMESPACE_METADATA, SEGMENT_PREFIX_ENCODED_SIZE,
    SegmentDirectory, SegmentDirectoryEntry, SegmentFooter, SegmentId, SegmentIndexEntry,
    SegmentIndexKey, SegmentIndexPage, SegmentRecordError, decode_segment_directory,
    decode_segment_footer, decode_segment_index_page, decode_segment_prefix,
    encode_segment_directory, encode_segment_footer, encode_segment_index_page,
    encode_segment_prefix, validate_canonical_object,
};

pub(crate) const MAX_SEGMENT_ENTRIES: usize = 1024;
pub(crate) const MAX_SEGMENT_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;
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
    #[error("stored metadata is not a canonical Wheat object: {0}")]
    InvalidMetadata(String),
}

fn key(entry: &SegmentIndexEntry) -> (u8, [u8; 32]) {
    (entry.namespace, entry.digest)
}

fn directory_key(entry: &SegmentDirectoryEntry) -> (u8, [u8; 32]) {
    (entry.first_key.namespace, entry.first_key.digest)
}

/// Streaming builder: caller buffers are borrowed only for the duration of `push`.
pub(crate) struct SegmentBuilder {
    file: NamedTempFile,
    entries: Vec<SegmentIndexEntry>,
    payload_bytes: u64,
    body_hasher: blake3::Hasher,
}

impl SegmentBuilder {
    pub(crate) fn new(directory: impl AsRef<Path>) -> Result<Self, SegmentError> {
        let mut file = NamedTempFile::new_in(directory)?;
        let prefix = encode_segment_prefix()?;
        debug_assert_eq!(prefix.len(), SEGMENT_PREFIX_ENCODED_SIZE);
        file.write_all(&prefix)?;
        let mut body_hasher = blake3::Hasher::new();
        body_hasher.update(&prefix);
        Ok(Self {
            file,
            entries: Vec::new(),
            payload_bytes: 0,
            body_hasher,
        })
    }

    pub(crate) fn push(&mut self, record: SegmentRecord<'_>) -> Result<(), SegmentError> {
        if self.entries.len() == MAX_SEGMENT_ENTRIES {
            return Err(SegmentError::TooManyEntries);
        }
        if !((record.namespace == CONTENT_NAMESPACE && record.kind == 0)
            || (record.namespace == METADATA_NAMESPACE && (1..=15).contains(&record.kind)))
        {
            return Err(SegmentError::Format("namespace/kind"));
        }
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
        self.body_hasher.update(record.bytes);
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
        self.entries.sort_unstable_by_key(key);
        if self.entries.windows(2).any(|w| key(&w[0]) == key(&w[1])) {
            return Err(SegmentError::DuplicateKey);
        }

        let index_start = PREFIX_LEN + self.payload_bytes;
        let mut pages = Vec::new();
        for chunk in self.entries.chunks(MAX_INDEX_PAGE_ENTRIES) {
            let offset = self.file.stream_position()?;
            let page = encode_segment_index_page(&SegmentIndexPage {
                entries: chunk.to_vec(),
            })?;
            let page_digest = *blake3::hash(&page).as_bytes();
            self.file.write_all(&page)?;
            self.body_hasher.update(&page);
            pages.push(SegmentDirectoryEntry {
                first_key: SegmentIndexKey {
                    namespace: chunk[0].namespace,
                    digest: chunk[0].digest,
                },
                offset,
                length: page.len() as u64,
                page_digest,
            });
        }

        let directory_offset = self.file.stream_position()?;
        let directory = encode_segment_directory(&SegmentDirectory { pages })?;
        self.file.write_all(&directory)?;
        self.body_hasher.update(&directory);
        let directory_length = directory.len() as u64;
        let body_length = self.file.stream_position()?;
        debug_assert_eq!(body_length, directory_offset + directory_length);

        let body_digest = *self.body_hasher.finalize().as_bytes();
        let total_length = body_length
            .checked_add(FOOTER_LEN)
            .ok_or(SegmentError::TooLarge)?;
        let footer = SegmentFooter {
            total_length,
            object_count: self.entries.len() as u64,
            index_start,
            directory_offset,
            directory_length,
            body_digest,
        };
        let footer_bytes = encode_segment_footer(&footer)?;
        debug_assert_eq!(footer_bytes.len(), SEGMENT_FOOTER_ENCODED_SIZE);
        self.file.write_all(&footer_bytes)?;
        self.file.flush()?;

        Ok(SealedSegment {
            file: self.file,
            id: footer.segment_id(),
            metadata: metadata(&footer),
        })
    }
}

pub(crate) struct SegmentReader {
    file: File,
    id: SegmentId,
    metadata: SegmentMetadata,
    directory: Vec<SegmentDirectoryEntry>,
    entries_by_key: Vec<SegmentIndexEntry>,
}

impl SegmentReader {
    /// Opens and fully authenticates the body once. Payloads are not retained.
    pub(crate) fn open(mut file: File) -> Result<Self, SegmentError> {
        let actual_len = file.metadata()?.len();
        // Bound all subsequent allocations, including a forged directory length.
        const MAX_PHYSICAL_BYTES: u64 = MAX_SEGMENT_PAYLOAD_BYTES + 128 * 1024;
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
        all_entries.sort_unstable_by_key(|entry| entry.offset);
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
            directory,
            entries_by_key: all_entries,
        })
    }

    pub(crate) fn id(&self) -> SegmentId {
        self.id
    }

    pub(crate) fn metadata(&self) -> SegmentMetadata {
        self.metadata
    }

    /// Enumerates the authenticated index without reading object payloads.
    ///
    /// `open` has already authenticated the complete segment body. Pages are
    /// nevertheless checked again here so a file modified after opening cannot
    /// silently poison a rebuilt catalog index.
    pub(crate) fn entries(&mut self) -> Result<Vec<SegmentIndexEntry>, SegmentError> {
        let mut result = Vec::with_capacity(self.metadata.object_count as usize);
        for descriptor in &self.directory {
            let page = read_range(&mut self.file, descriptor.offset, descriptor.length)?;
            if blake3::hash(&page).as_bytes() != &descriptor.page_digest {
                return Err(SegmentError::PageDigest);
            }
            let entries = decode_segment_index_page(&page)?.entries;
            if result.len().checked_add(entries.len()).is_none_or(|count| {
                count > MAX_SEGMENT_ENTRIES || count as u64 > self.metadata.object_count
            }) {
                return Err(SegmentError::TooManyEntries);
            }
            result.extend(entries);
        }
        if result.len() as u64 != self.metadata.object_count {
            return Err(SegmentError::Format("object count"));
        }
        Ok(result)
    }

    pub(crate) fn lookup(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, SegmentError> {
        let wanted = (namespace, digest);
        let Ok(index) = self.entries_by_key.binary_search_by_key(&wanted, key) else {
            return Ok(None);
        };
        let entry = &self.entries_by_key[index];
        let bytes = read_range(&mut self.file, entry.offset, entry.length)?;
        verify_logical(entry, &bytes)?;
        Ok(Some((entry.kind, bytes)))
    }
}

fn metadata(footer: &SegmentFooter) -> SegmentMetadata {
    SegmentMetadata {
        object_count: footer.object_count,
        payload_bytes: footer.index_start - PREFIX_LEN,
        total_length: footer.total_length,
        index_start: footer.index_start,
        directory_offset: footer.directory_offset,
        directory_length: footer.directory_length,
    }
}

fn verify_logical(entry: &SegmentIndexEntry, bytes: &[u8]) -> Result<(), SegmentError> {
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
            let kind = validate_canonical_object(bytes)
                .map_err(|error| SegmentError::InvalidMetadata(error.to_string()))?;
            if canonical_kind_code(kind) != entry.kind {
                return Err(SegmentError::WrongKind);
            }
        }
        _ => return Err(SegmentError::Format("namespace")),
    }
    Ok(())
}

fn canonical_kind_code(kind: CanonicalObjectKind) -> u16 {
    match kind {
        CanonicalObjectKind::Tree => 1,
        CanonicalObjectKind::Change => 2,
        CanonicalObjectKind::Revision => 3,
        CanonicalObjectKind::Application => 4,
        CanonicalObjectKind::Conflict => 5,
        CanonicalObjectKind::View => 6,
        CanonicalObjectKind::ContentManifest => 7,
        CanonicalObjectKind::RepositoryState => 8,
        CanonicalObjectKind::RevisionMapNode => 9,
        CanonicalObjectKind::ApplicationMapNode => 10,
        CanonicalObjectKind::ConflictSetNode => 11,
        CanonicalObjectKind::ContentSetNode => 12,
        CanonicalObjectKind::RetainedSetNode => 13,
        CanonicalObjectKind::BranchManifest => 14,
        CanonicalObjectKind::RepositoryEnvelope => 15,
    }
}

fn hash_range(file: &mut File, offset: u64, length: u64) -> Result<[u8; 32], SegmentError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    while remaining != 0 {
        let amount = usize::try_from(remaining.min(HASH_BUFFER_SIZE as u64)).unwrap();
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
    fn rejects_wrong_namespace_and_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(directory.path()).unwrap();
        assert!(matches!(
            builder.push(SegmentRecord {
                namespace: 0,
                kind: 0,
                digest: [0; 32],
                bytes: b"x"
            }),
            Err(SegmentError::Format("namespace/kind"))
        ));

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
