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
const INDEX_ENTRY_SIZE: usize = 51;
const DIRECTORY_ENTRY_SIZE: usize = 81;

pub const SEGMENT_NAMESPACE_METADATA: u8 = 1;
pub const SEGMENT_NAMESPACE_CONTENT: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SegmentId(pub [u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentIndexEntry {
    pub namespace: u8,
    /// Zero for content; otherwise the numeric `ObjectKind` tag.
    pub kind: u16,
    pub digest: [u8; 32],
    pub offset: u64,
    /// Stored and logical lengths are identical until codecs are introduced.
    pub length: u64,
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
            crate::CanonicalObjectKind::RetainedSetNode => 13,
            crate::CanonicalObjectKind::BranchManifest => 14,
            crate::CanonicalObjectKind::RepositoryEnvelope => 15,
            crate::CanonicalObjectKind::Edit => 16,
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
            13 => crate::CanonicalObjectKind::RetainedSetNode,
            14 => crate::CanonicalObjectKind::BranchManifest,
            15 => crate::CanonicalObjectKind::RepositoryEnvelope,
            16 => crate::CanonicalObjectKind::Edit,
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
        length: e.length,
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
            length: e.length,
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
