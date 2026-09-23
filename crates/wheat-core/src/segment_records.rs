//! Safe domain wrappers for Wheat's physical sealed-segment records.
//!
//! These records are intentionally separate from the canonical `WheatObject`
//! union. A segment ID is the footer's body digest: the hash computed by the
//! segment writer over every encoded byte preceding the footer. Decoding a
//! footer only validates its record encoding; the segment reader is responsible
//! for checking that hash and all cross-record counts, lengths, and offsets.

use thiserror::Error;
use wheat_binschema::generated as wire;

pub const SEGMENT_PREFIX_ENCODED_SIZE: usize = 6;
pub const SEGMENT_FOOTER_ENCODED_SIZE: usize = 78;
const INDEX_ENTRY_SIZE: usize = 60;
const DIRECTORY_ENTRY_SIZE: usize = 81;

pub const SEGMENT_NAMESPACE_METADATA: u8 = 1;
pub const SEGMENT_NAMESPACE_CONTENT: u8 = 2;

/// Physical payload codec.
///
/// `Raw` and `Zstd` are per-record codecs (each record's payload is
/// independently encoded). `ZstdBlock` groups multiple small records into one
/// zstd frame: records within a block share the same `(offset, stored_length)`
/// in their index entries. The reader decompresses the block and extracts each
/// record at the logical position derived from co-block entries' `logical_length`
/// values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum SegmentCodec {
    Raw = 0,
    Zstd = 1,
    ZstdBlock = 4,
}

impl SegmentCodec {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            4 => Some(Self::ZstdBlock),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SegmentId(pub [u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentIndexEntry {
    pub namespace: u8,
    /// Zero for content; otherwise the numeric `ObjectKind` tag.
    pub kind: u16,
    pub digest: [u8; 32],
    pub offset: u64,
    /// [`SegmentCodec`] numeric identity (`Raw = 0`, `Zstd = 1`, `ZstdBlock = 4`).
    pub codec: u8,
    pub stored_length: u64,
    pub logical_length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentIndexPage {
    pub entries: Vec<SegmentIndexEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentIndexKey {
    pub namespace: u8,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentDirectoryEntry {
    pub first_key: SegmentIndexKey,
    pub offset: u64,
    pub length: u64,
    pub page_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentDirectory {
    pub pages: Vec<SegmentDirectoryEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentFooter {
    pub total_length: u64,
    pub object_count: u64,
    pub index_start: u64,
    pub directory_offset: u64,
    pub directory_length: u64,
    /// Writer-computed digest of all encoded segment bytes before the footer.
    pub body_digest: [u8; 32],
}

/// Physical publication record for one immutable sealed segment.
///
/// This record is deliberately outside the logical `WheatObject` union.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentCatalogRecord {
    pub generation: u64,
    pub previous: Option<[u8; 32]>,
    pub segment: [u8; 32],
    pub stored_length: u64,
}

/// One immutable segment named by a V2 physical catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalSegmentDescriptor {
    /// Physical generation used in the canonical segment filename.
    pub generation: u64,
    /// Stable position among segments created in that generation.
    pub sequence: u64,
    pub segment: [u8; 32],
    pub stored_length: u64,
}

/// Content-envelope authority selected alongside structural segments.
///
/// Generation zero with an all-zero manifest denotes the legacy content layout.
/// Store-side structural repack deliberately preserves that layout unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalContentGeneration {
    pub generation: u64,
    pub manifest: [u8; 32],
}

/// Complete authenticated physical authority for a local packed store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalCatalogSnapshot {
    pub generation: u64,
    pub segments: Vec<PhysicalSegmentDescriptor>,
    pub content: PhysicalContentGeneration,
}

/// Hard allocation bound for an authenticated V2 catalog snapshot.
pub const MAX_PHYSICAL_CATALOG_SEGMENTS: usize = 1_000_000;
const PHYSICAL_CATALOG_MAGIC: [u8; 4] = 0x5748_4354_u32.to_be_bytes();
const PHYSICAL_CATALOG_V2: u16 = 2;

impl SegmentFooter {
    pub fn segment_id(&self) -> SegmentId {
        SegmentId(self.body_digest)
    }
}

impl SegmentIndexEntry {
    /// Returns the stable segment kind code for a canonical metadata object.
    pub fn canonical_kind_code(kind: crate::CanonicalObjectKind) -> u16 {
        match kind {
            crate::CanonicalObjectKind::Tree => 1,
            crate::CanonicalObjectKind::Change => 2,
            crate::CanonicalObjectKind::Revision => 3,
            crate::CanonicalObjectKind::Application => 4,
            crate::CanonicalObjectKind::Conflict => 5,
            crate::CanonicalObjectKind::View => 6,
            crate::CanonicalObjectKind::ContentManifest => 7,
            crate::CanonicalObjectKind::RepositoryState => 8,
            crate::CanonicalObjectKind::RevisionMapNode => 9,
            crate::CanonicalObjectKind::ApplicationMapNode => 10,
            crate::CanonicalObjectKind::ConflictSetNode => 11,
            crate::CanonicalObjectKind::ContentSetNode => 12,
            crate::CanonicalObjectKind::ContentSetSnapshot => 19,
            crate::CanonicalObjectKind::ContentSetDelta => 20,
            crate::CanonicalObjectKind::RetainedSetNode => 13,
            crate::CanonicalObjectKind::ViewLabelManifest => 14,
            crate::CanonicalObjectKind::RepositoryEnvelope => 15,
            crate::CanonicalObjectKind::Edit => 16,
            crate::CanonicalObjectKind::ChangeMapNode => 17,
            crate::CanonicalObjectKind::ViewLineageMapNode => 18,
        }
    }

    /// Resolves a segment metadata kind code, rejecting unassigned values.
    pub fn canonical_kind(code: u16) -> Option<crate::CanonicalObjectKind> {
        Some(match code {
            1 => crate::CanonicalObjectKind::Tree,
            2 => crate::CanonicalObjectKind::Change,
            3 => crate::CanonicalObjectKind::Revision,
            4 => crate::CanonicalObjectKind::Application,
            5 => crate::CanonicalObjectKind::Conflict,
            6 => crate::CanonicalObjectKind::View,
            7 => crate::CanonicalObjectKind::ContentManifest,
            8 => crate::CanonicalObjectKind::RepositoryState,
            9 => crate::CanonicalObjectKind::RevisionMapNode,
            10 => crate::CanonicalObjectKind::ApplicationMapNode,
            11 => crate::CanonicalObjectKind::ConflictSetNode,
            12 => crate::CanonicalObjectKind::ContentSetNode,
            19 => crate::CanonicalObjectKind::ContentSetSnapshot,
            20 => crate::CanonicalObjectKind::ContentSetDelta,
            13 => crate::CanonicalObjectKind::RetainedSetNode,
            14 => crate::CanonicalObjectKind::ViewLabelManifest,
            15 => crate::CanonicalObjectKind::RepositoryEnvelope,
            16 => crate::CanonicalObjectKind::Edit,
            17 => crate::CanonicalObjectKind::ChangeMapNode,
            18 => crate::CanonicalObjectKind::ViewLineageMapNode,
            _ => return None,
        })
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SegmentRecordError {
    #[error("segment record does not decode: {0}")]
    Decode(String),
    #[error("segment record is not exactly and canonically encoded")]
    NonCanonicalEncoding,
    #[error("invalid segment index namespace/kind combination ({namespace}, {kind})")]
    InvalidNamespaceKind { namespace: u8, kind: u16 },
    #[error("invalid segment payload codec {0}")]
    InvalidCodec(u8),
    #[error("raw segment payload has unequal stored and logical lengths")]
    InvalidRawLengths,
    #[error("segment record count cannot fit in supplied bytes")]
    InvalidCount,
}

fn digest(bytes: [u8; 32]) -> wire::Digest256 {
    wire::Digest256 {
        bytes: bytes.to_vec(),
    }
}
fn digest_array(value: wire::Digest256) -> Result<[u8; 32], SegmentRecordError> {
    value
        .bytes
        .try_into()
        .map_err(|_| SegmentRecordError::Decode("digest is not 32 bytes".into()))
}
fn checked<T, E: std::fmt::Debug>(
    bytes: &[u8],
    value: T,
    encoded: Result<Vec<u8>, E>,
) -> Result<T, SegmentRecordError> {
    let encoded = encoded.map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    if encoded != bytes {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    Ok(value)
}
fn valid_entry(e: &SegmentIndexEntry) -> Result<(), SegmentRecordError> {
    if !((e.namespace == SEGMENT_NAMESPACE_CONTENT && e.kind == 0)
        || (e.namespace == SEGMENT_NAMESPACE_METADATA
            && SegmentIndexEntry::canonical_kind(e.kind).is_some()))
    {
        return Err(SegmentRecordError::InvalidNamespaceKind {
            namespace: e.namespace,
            kind: e.kind,
        });
    }
    let codec =
        SegmentCodec::from_code(e.codec).ok_or(SegmentRecordError::InvalidCodec(e.codec))?;
    if codec == SegmentCodec::Raw && e.stored_length != e.logical_length {
        return Err(SegmentRecordError::InvalidRawLengths);
    }
    // ZstdBlock entries: stored_length is the compressed block size (shared by
    // all entries in the block), logical_length is this record's decompressed
    // size. No per-entry length invariant to check.
    Ok(())
}

pub fn encode_segment_prefix() -> Result<Vec<u8>, SegmentRecordError> {
    wire::SegmentPrefixV1Input {}
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn decode_segment_prefix(bytes: &[u8]) -> Result<(), SegmentRecordError> {
    let v = wire::SegmentPrefixV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    checked(bytes, (), v.encode())
}

fn entry_wire(e: &SegmentIndexEntry) -> wire::SegmentIndexEntryV1 {
    wire::SegmentIndexEntryV1 {
        namespace: e.namespace,
        kind: e.kind,
        digest: digest(e.digest),
        offset: e.offset,
        codec: e.codec,
        stored_length: e.stored_length,
        logical_length: e.logical_length,
    }
}
pub fn encode_segment_index_page(page: &SegmentIndexPage) -> Result<Vec<u8>, SegmentRecordError> {
    for e in &page.entries {
        valid_entry(e)?;
    }
    wire::SegmentIndexPageV1 {
        entries: page.entries.iter().map(entry_wire).collect(),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn decode_segment_index_page(bytes: &[u8]) -> Result<SegmentIndexPage, SegmentRecordError> {
    preflight(bytes, INDEX_ENTRY_SIZE)?;
    let w = wire::SegmentIndexPageV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let mut entries = Vec::with_capacity(w.entries.len());
    for e in w.entries {
        let e = SegmentIndexEntry {
            namespace: e.namespace,
            kind: e.kind,
            digest: digest_array(e.digest)?,
            offset: e.offset,
            codec: e.codec,
            stored_length: e.stored_length,
            logical_length: e.logical_length,
        };
        valid_entry(&e)?;
        entries.push(e);
    }
    checked(bytes, SegmentIndexPage { entries }, encoded)
}

fn preflight(bytes: &[u8], item_size: usize) -> Result<(), SegmentRecordError> {
    if bytes.len() < 4 {
        return Err(SegmentRecordError::InvalidCount);
    }
    let count = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if count > (bytes.len() - 4) / item_size
        || 4usize
            .checked_add(
                count
                    .checked_mul(item_size)
                    .ok_or(SegmentRecordError::InvalidCount)?,
            )
            .ok_or(SegmentRecordError::InvalidCount)?
            != bytes.len()
    {
        return Err(SegmentRecordError::InvalidCount);
    }
    Ok(())
}

pub fn encode_segment_directory(
    directory: &SegmentDirectory,
) -> Result<Vec<u8>, SegmentRecordError> {
    let pages = directory
        .pages
        .iter()
        .map(|p| wire::SegmentDirectoryEntryV1 {
            first_key: wire::SegmentIndexKeyV1 {
                namespace: p.first_key.namespace,
                digest: digest(p.first_key.digest),
            },
            offset: p.offset,
            length: p.length,
            page_digest: digest(p.page_digest),
        })
        .collect();
    wire::SegmentDirectoryV1 { pages }
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn decode_segment_directory(bytes: &[u8]) -> Result<SegmentDirectory, SegmentRecordError> {
    preflight(bytes, DIRECTORY_ENTRY_SIZE)?;
    let w = wire::SegmentDirectoryV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let mut pages = Vec::with_capacity(w.pages.len());
    for p in w.pages {
        pages.push(SegmentDirectoryEntry {
            first_key: SegmentIndexKey {
                namespace: p.first_key.namespace,
                digest: digest_array(p.first_key.digest)?,
            },
            offset: p.offset,
            length: p.length,
            page_digest: digest_array(p.page_digest)?,
        });
    }
    checked(bytes, SegmentDirectory { pages }, encoded)
}

pub fn encode_segment_footer(f: &SegmentFooter) -> Result<Vec<u8>, SegmentRecordError> {
    wire::SegmentFooterV1Input {
        total_length: f.total_length,
        object_count: f.object_count,
        index_start: f.index_start,
        directory_offset: f.directory_offset,
        directory_length: f.directory_length,
        body_digest: digest(f.body_digest),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn decode_segment_footer(bytes: &[u8]) -> Result<SegmentFooter, SegmentRecordError> {
    if bytes.len() != SEGMENT_FOOTER_ENCODED_SIZE {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    let w = wire::SegmentFooterV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let f = SegmentFooter {
        total_length: w.total_length,
        object_count: w.object_count,
        index_start: w.index_start,
        directory_offset: w.directory_offset,
        directory_length: w.directory_length,
        body_digest: digest_array(w.body_digest)?,
    };
    checked(bytes, f, encoded)
}

pub fn encode_segment_catalog_record(
    record: &SegmentCatalogRecord,
) -> Result<Vec<u8>, SegmentRecordError> {
    wire::SegmentCatalogRecordV1Input {
        generation: record.generation,
        previous: record.previous.map(digest),
        segment: digest(record.segment),
        stored_length: record.stored_length,
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}

pub fn decode_segment_catalog_record(
    bytes: &[u8],
) -> Result<SegmentCatalogRecord, SegmentRecordError> {
    let w = wire::SegmentCatalogRecordV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let record = SegmentCatalogRecord {
        generation: w.generation,
        previous: w.previous.map(digest_array).transpose()?,
        segment: digest_array(w.segment)?,
        stored_length: w.stored_length,
    };
    checked(bytes, record, encoded)
}

/// Canonically encodes a complete V2 physical catalog snapshot.
///
/// The small hand-written envelope intentionally shares the existing catalog
/// magic while retaining the generated V1 decoder unchanged. Integer fields are
/// big-endian and descriptors must be sorted by contiguous sequence number.
pub fn encode_physical_catalog_snapshot(
    snapshot: &PhysicalCatalogSnapshot,
) -> Result<Vec<u8>, SegmentRecordError> {
    validate_physical_catalog_snapshot(snapshot)?;
    let count =
        u32::try_from(snapshot.segments.len()).map_err(|_| SegmentRecordError::InvalidCount)?;
    let capacity = 4 + 2 + 8 + 4 + snapshot.segments.len() * 56 + 8 + 32;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&PHYSICAL_CATALOG_MAGIC);
    encoded.extend_from_slice(&PHYSICAL_CATALOG_V2.to_be_bytes());
    encoded.extend_from_slice(&snapshot.generation.to_be_bytes());
    encoded.extend_from_slice(&count.to_be_bytes());
    for descriptor in &snapshot.segments {
        encoded.extend_from_slice(&descriptor.generation.to_be_bytes());
        encoded.extend_from_slice(&descriptor.sequence.to_be_bytes());
        encoded.extend_from_slice(&descriptor.segment);
        encoded.extend_from_slice(&descriptor.stored_length.to_be_bytes());
    }
    encoded.extend_from_slice(&snapshot.content.generation.to_be_bytes());
    encoded.extend_from_slice(&snapshot.content.manifest);
    Ok(encoded)
}

/// Decodes and canonical-validates a complete V2 physical catalog snapshot.
pub fn decode_physical_catalog_snapshot(
    bytes: &[u8],
) -> Result<PhysicalCatalogSnapshot, SegmentRecordError> {
    const HEADER: usize = 4 + 2 + 8 + 4;
    const DESCRIPTOR: usize = 8 + 8 + 32 + 8;
    const CONTENT: usize = 8 + 32;
    if bytes.len() < HEADER + CONTENT
        || bytes[..4] != PHYSICAL_CATALOG_MAGIC
        || u16::from_be_bytes(bytes[4..6].try_into().unwrap()) != PHYSICAL_CATALOG_V2
    {
        return Err(SegmentRecordError::Decode(
            "not a V2 physical catalog snapshot".into(),
        ));
    }
    let generation = u64::from_be_bytes(bytes[6..14].try_into().unwrap());
    let count = u32::from_be_bytes(bytes[14..18].try_into().unwrap()) as usize;
    if count > MAX_PHYSICAL_CATALOG_SEGMENTS
        || HEADER
            .checked_add(
                count
                    .checked_mul(DESCRIPTOR)
                    .ok_or(SegmentRecordError::InvalidCount)?,
            )
            .and_then(|length| length.checked_add(CONTENT))
            != Some(bytes.len())
    {
        return Err(SegmentRecordError::InvalidCount);
    }
    let mut segments = Vec::with_capacity(count);
    let mut offset = HEADER;
    for _ in 0..count {
        segments.push(PhysicalSegmentDescriptor {
            generation: u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            sequence: u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().unwrap()),
            segment: bytes[offset + 16..offset + 48].try_into().unwrap(),
            stored_length: u64::from_be_bytes(bytes[offset + 48..offset + 56].try_into().unwrap()),
        });
        offset += DESCRIPTOR;
    }
    let snapshot = PhysicalCatalogSnapshot {
        generation,
        segments,
        content: PhysicalContentGeneration {
            generation: u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            manifest: bytes[offset + 8..offset + 40].try_into().unwrap(),
        },
    };
    validate_physical_catalog_snapshot(&snapshot)?;
    if encode_physical_catalog_snapshot(&snapshot)? != bytes {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    Ok(snapshot)
}

fn validate_physical_catalog_snapshot(
    snapshot: &PhysicalCatalogSnapshot,
) -> Result<(), SegmentRecordError> {
    if snapshot.segments.len() > MAX_PHYSICAL_CATALOG_SEGMENTS {
        return Err(SegmentRecordError::InvalidCount);
    }
    if snapshot.segments.windows(2).any(|pair| {
        (pair[0].generation, pair[0].sequence) >= (pair[1].generation, pair[1].sequence)
    }) || snapshot
        .segments
        .iter()
        .any(|descriptor| descriptor.stored_length == 0)
    {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    if snapshot
        .segments
        .windows(2)
        .any(|pair| pair[0].segment == pair[1].segment)
    {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    Ok(())
}
