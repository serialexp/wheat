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
/// Encoded size of a whole-record index entry; a delta entry adds the
/// 32-byte base digest.
pub const SEGMENT_INDEX_ENTRY_ENCODED_SIZE: usize = 62;
pub const SEGMENT_DELTA_INDEX_ENTRY_ENCODED_SIZE: usize = SEGMENT_INDEX_ENTRY_ENCODED_SIZE + 32;
/// Longest chain of in-segment metadata deltas above a whole record.
pub const SEGMENT_MAX_DELTA_DEPTH: u8 = 16;
pub const ACTIVE_LOG_HEADER_ENCODED_SIZE: usize = 14;
pub const ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE: usize = 60;
pub const ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE: usize = 36;

pub const SEGMENT_NAMESPACE_METADATA: u8 = 1;
pub const SEGMENT_NAMESPACE_CONTENT: u8 = 2;

/// Physical payload codec.
///
/// `Raw` and `Zstd` are per-record codecs (each record's payload is
/// independently encoded). `Delta` and `DeltaZstd` store a metadata record as a
/// git delta patch (optionally one zstd frame) against the entry's
/// `delta_base`, a record in the same segment. `ZstdBlock` groups multiple small records into one
/// zstd frame: records within a block share the same `(offset, stored_length)`
/// in their index entries. The reader decompresses the block and extracts each
/// record at the logical position derived from co-block entries' `logical_length`
/// values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum SegmentCodec {
    Raw = 0,
    Zstd = 1,
    Delta = 2,
    DeltaZstd = 3,
    ZstdBlock = 4,
}

impl SegmentCodec {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            2 => Some(Self::Delta),
            3 => Some(Self::DeltaZstd),
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
    /// [`SegmentCodec`] numeric identity.
    pub codec: u8,
    pub stored_length: u64,
    pub logical_length: u64,
    /// Base record (same segment, metadata namespace) of a delta codec entry.
    pub delta_base: Option<[u8; 32]>,
    /// Zero for whole records; base depth plus one for deltas.
    pub delta_depth: u8,
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

/// Remote packed-store publication record for one appended segment.
///
/// This record is deliberately outside the logical `WheatObject` union.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentCatalogRecord {
    pub generation: u64,
    pub previous: Option<[u8; 32]>,
    pub segment: [u8; 32],
    pub stored_length: u64,
}

/// One immutable segment named by a physical catalog snapshot.
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
/// Generation zero with an all-zero manifest means no content generation has
/// been selected yet: content still lives in segments and loose envelopes, as
/// written by import and commits before the first repack.
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

/// One catalog snapshot by name: its generation and the BLAKE3 digest of its
/// encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalCatalogRef {
    pub generation: u64,
    pub digest: [u8; 32],
}

/// A repack about to move the catalog head from `old` to `new`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepackIntent {
    pub old: Option<PhysicalCatalogRef>,
    pub new: PhysicalCatalogRef,
}

/// Hard allocation bound for an authenticated catalog snapshot.
pub const MAX_PHYSICAL_CATALOG_SEGMENTS: usize = 1_000_000;

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
            crate::CanonicalObjectKind::ContentMapNode => 7,
            crate::CanonicalObjectKind::RepositoryState => 8,
            crate::CanonicalObjectKind::RevisionMapNode => 9,
            crate::CanonicalObjectKind::ApplicationMapNode => 10,
            crate::CanonicalObjectKind::ConflictSetNode => 11,
            crate::CanonicalObjectKind::ContentSetNode => 12,
            crate::CanonicalObjectKind::RetainedSetNode => 13,
            crate::CanonicalObjectKind::ViewLabelManifest => 14,
            crate::CanonicalObjectKind::RepositoryEnvelope => 15,
            crate::CanonicalObjectKind::Edit => 16,
            crate::CanonicalObjectKind::ChangeMapNode => 17,
            crate::CanonicalObjectKind::ViewLineageMapNode => 18,
            // 19 and 20 were the removed content-set snapshot and delta kinds.
            crate::CanonicalObjectKind::ProjectedCommitSetNode => 21,
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
            7 => crate::CanonicalObjectKind::ContentMapNode,
            8 => crate::CanonicalObjectKind::RepositoryState,
            9 => crate::CanonicalObjectKind::RevisionMapNode,
            10 => crate::CanonicalObjectKind::ApplicationMapNode,
            11 => crate::CanonicalObjectKind::ConflictSetNode,
            12 => crate::CanonicalObjectKind::ContentSetNode,
            13 => crate::CanonicalObjectKind::RetainedSetNode,
            14 => crate::CanonicalObjectKind::ViewLabelManifest,
            15 => crate::CanonicalObjectKind::RepositoryEnvelope,
            16 => crate::CanonicalObjectKind::Edit,
            17 => crate::CanonicalObjectKind::ChangeMapNode,
            18 => crate::CanonicalObjectKind::ViewLineageMapNode,
            21 => crate::CanonicalObjectKind::ProjectedCommitSetNode,
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
    #[error("segment delta fields do not match the entry codec, namespace, or depth limit")]
    InvalidDelta,
    #[error("segment record count cannot fit in supplied bytes")]
    InvalidCount,
    #[error("record digest does not match its fields")]
    DigestMismatch,
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
    let is_delta = matches!(codec, SegmentCodec::Delta | SegmentCodec::DeltaZstd);
    let delta_valid = if is_delta {
        e.namespace == SEGMENT_NAMESPACE_METADATA
            && e.delta_base.is_some_and(|base| base != e.digest)
            && (1..=SEGMENT_MAX_DELTA_DEPTH).contains(&e.delta_depth)
    } else {
        e.delta_base.is_none() && e.delta_depth == 0
    };
    if !delta_valid {
        return Err(SegmentRecordError::InvalidDelta);
    }
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
        delta_base: e.delta_base.map(digest),
        delta_depth: e.delta_depth,
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
            delta_base: e.delta_base.map(digest_array).transpose()?,
            delta_depth: e.delta_depth,
        };
        valid_entry(&e)?;
        entries.push(e);
    }
    checked(bytes, SegmentIndexPage { entries }, encoded)
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

/// The fields of one active-log frame, which its header authenticates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveLogFrameFields {
    pub generation: u64,
    pub payload_length: u64,
    pub index_length: u64,
}

pub fn encode_active_log_header(base_generation: u64) -> Result<Vec<u8>, SegmentRecordError> {
    wire::ActiveLogHeaderV1Input { base_generation }
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
/// Decodes an active-log header to its base generation.
pub fn decode_active_log_header(bytes: &[u8]) -> Result<u64, SegmentRecordError> {
    if bytes.len() != ACTIVE_LOG_HEADER_ENCODED_SIZE {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    let w = wire::ActiveLogHeaderV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    checked(bytes, w.base_generation, w.encode())
}

/// The encoded fields alone, which the header's `fields_digest` covers.
pub fn encode_active_log_frame_fields(
    fields: &ActiveLogFrameFields,
) -> Result<Vec<u8>, SegmentRecordError> {
    frame_fields_wire(fields)
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn encode_active_log_frame_header(
    fields: &ActiveLogFrameFields,
    fields_digest: [u8; 32],
) -> Result<Vec<u8>, SegmentRecordError> {
    wire::ActiveLogFrameHeaderV1 {
        fields: frame_fields_wire(fields).into(),
        fields_digest: digest(fields_digest),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
/// Decodes a frame header to its fields and their digest.
pub fn decode_active_log_frame_header(
    bytes: &[u8],
) -> Result<(ActiveLogFrameFields, [u8; 32]), SegmentRecordError> {
    if bytes.len() != ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    let w = wire::ActiveLogFrameHeaderV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let fields = ActiveLogFrameFields {
        generation: w.fields.generation,
        payload_length: w.fields.payload_length,
        index_length: w.fields.index_length,
    };
    checked(bytes, (fields, digest_array(w.fields_digest)?), encoded)
}
fn frame_fields_wire(fields: &ActiveLogFrameFields) -> wire::ActiveLogFrameFieldsV1Input {
    wire::ActiveLogFrameFieldsV1Input {
        generation: fields.generation,
        payload_length: fields.payload_length,
        index_length: fields.index_length,
    }
}

pub fn encode_active_log_frame_trailer(
    chain_digest: [u8; 32],
) -> Result<Vec<u8>, SegmentRecordError> {
    wire::ActiveLogFrameTrailerV1Input {
        chain_digest: digest(chain_digest),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
/// Decodes a frame trailer to its chain digest.
pub fn decode_active_log_frame_trailer(bytes: &[u8]) -> Result<[u8; 32], SegmentRecordError> {
    if bytes.len() != ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE {
        return Err(SegmentRecordError::NonCanonicalEncoding);
    }
    let w = wire::ActiveLogFrameTrailerV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    checked(bytes, digest_array(w.chain_digest)?, encoded)
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

/// Canonically encodes a complete physical catalog snapshot.
pub fn encode_physical_catalog_snapshot(
    snapshot: &PhysicalCatalogSnapshot,
) -> Result<Vec<u8>, SegmentRecordError> {
    validate_physical_catalog_snapshot(snapshot)?;
    wire::PhysicalCatalogSnapshotV1Input {
        generation: snapshot.generation,
        segments: snapshot
            .segments
            .iter()
            .map(|descriptor| wire::PhysicalSegmentDescriptorV1 {
                generation: descriptor.generation,
                sequence: descriptor.sequence,
                segment: digest(descriptor.segment),
                stored_length: descriptor.stored_length,
            })
            .collect(),
        content: wire::PhysicalContentGenerationV1 {
            generation: snapshot.content.generation,
            manifest: digest(snapshot.content.manifest),
        },
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}

/// Decodes and canonical-validates a complete physical catalog snapshot.
pub fn decode_physical_catalog_snapshot(
    bytes: &[u8],
) -> Result<PhysicalCatalogSnapshot, SegmentRecordError> {
    let w = wire::PhysicalCatalogSnapshotV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let snapshot = PhysicalCatalogSnapshot {
        generation: w.generation,
        segments: w
            .segments
            .into_iter()
            .map(|descriptor| {
                Ok(PhysicalSegmentDescriptor {
                    generation: descriptor.generation,
                    sequence: descriptor.sequence,
                    segment: digest_array(descriptor.segment)?,
                    stored_length: descriptor.stored_length,
                })
            })
            .collect::<Result<_, SegmentRecordError>>()?,
        content: PhysicalContentGeneration {
            generation: w.content.generation,
            manifest: digest_array(w.content.manifest)?,
        },
    };
    validate_physical_catalog_snapshot(&snapshot)?;
    checked(bytes, snapshot, encoded)
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

fn catalog_ref_wire(head: PhysicalCatalogRef) -> wire::PhysicalCatalogRefV1 {
    wire::PhysicalCatalogRefV1 {
        generation: head.generation,
        digest: digest(head.digest),
    }
}
fn catalog_ref_domain(
    head: wire::PhysicalCatalogRefV1,
) -> Result<PhysicalCatalogRef, SegmentRecordError> {
    Ok(PhysicalCatalogRef {
        generation: head.generation,
        digest: digest_array(head.digest)?,
    })
}

/// Encodes `catalog.head`. `None` is a store that has not sealed anything yet.
pub fn encode_physical_catalog_head(
    head: Option<PhysicalCatalogRef>,
) -> Result<Vec<u8>, SegmentRecordError> {
    wire::PhysicalCatalogHeadV1Input {
        head: head.map(catalog_ref_wire),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
pub fn decode_physical_catalog_head(
    bytes: &[u8],
) -> Result<Option<PhysicalCatalogRef>, SegmentRecordError> {
    let w = wire::PhysicalCatalogHeadV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let head = w.head.map(catalog_ref_domain).transpose()?;
    checked(bytes, head, encoded)
}

/// Encodes `repack.intent` with the digest of its fields.
pub fn encode_repack_intent(intent: &RepackIntent) -> Result<Vec<u8>, SegmentRecordError> {
    let fields: wire::RepackIntentFieldsV1 = wire::RepackIntentFieldsV1Input {
        old: intent.old.map(catalog_ref_wire),
        new: catalog_ref_wire(intent.new),
    }
    .into();
    let encoded = fields
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    wire::RepackIntentV1 {
        fields,
        fields_digest: digest(*blake3::hash(&encoded).as_bytes()),
    }
    .encode()
    .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))
}
/// Decodes `repack.intent` and checks the digest of its fields.
pub fn decode_repack_intent(bytes: &[u8]) -> Result<RepackIntent, SegmentRecordError> {
    let w = wire::RepackIntentV1::decode(bytes)
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    let encoded = w.encode();
    let fields = w
        .fields
        .encode()
        .map_err(|e| SegmentRecordError::Decode(format!("{e:?}")))?;
    if blake3::hash(&fields).as_bytes() != &digest_array(w.fields_digest)? {
        return Err(SegmentRecordError::DigestMismatch);
    }
    let intent = RepackIntent {
        old: w.fields.old.map(catalog_ref_domain).transpose()?,
        new: catalog_ref_domain(w.fields.new)?,
    };
    checked(bytes, intent, encoded)
}
