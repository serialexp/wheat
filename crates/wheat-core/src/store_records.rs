//! Safe domain wrappers for the stores' small persisted records: the repository
//! head, the content envelope header, snapshot and ownership manifests, a
//! workspace materialization's progress log, and a Git import's source binding.
//!
//! Like the physical segment records, none of these are `WheatObject` variants.

use thiserror::Error;
use wheat_binschema::generated as wire;

use crate::RepositoryEnvelopeId;

/// Encoded size of a repository head with no publish receipts.
pub const REPOSITORY_HEAD_ENCODED_SIZE: usize = 48;
/// Encoded size of one publish receipt in a repository head.
pub const PUBLISH_RECEIPT_ENCODED_SIZE: usize = 96;
/// Most publish receipts a repository head holds.
pub const MAX_PUBLISH_RECEIPTS: usize = 1024;
pub const CONTENT_ENVELOPE_HEADER_ENCODED_SIZE: usize = 22;
pub const MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE: usize = 36;
/// Encoded size of one inventory entry in a snapshot or ownership manifest.
pub const INVENTORY_ENTRY_ENCODED_SIZE: usize = 40;
/// Encoded size of a snapshot manifest with empty inventories.
pub const SNAPSHOT_MANIFEST_FIXED_ENCODED_SIZE: usize = 86;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum StoreRecordError {
    #[error("store record does not decode: {0}")]
    Decode(String),
    #[error("store record is not exactly and canonically encoded")]
    NonCanonicalEncoding,
    #[error("store record count exceeds its limit or the supplied bytes")]
    InvalidCount,
}

/// The published generation and its repository envelope, and the receipts of recent
/// remote publishes, oldest first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryHeadRecord {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
    pub receipts: Vec<PublishReceiptRecord>,
}

/// A publish the server made durable, kept so a retry with the same key gets the
/// same answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishReceiptRecord {
    /// The client's publish key, bound to its principal.
    pub key: [u8; 16],
    /// A digest of what the publish asked for.
    pub request: [u8; 16],
    pub receipt: [u8; 16],
    /// The head the publish was published in.
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
    /// Seconds since the Unix epoch.
    pub recorded_at: u64,
}

/// The fixed header in front of a content envelope's stored payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentEnvelopeHeaderRecord {
    pub codec: u8,
    pub stored_length: u64,
    pub logical_length: u64,
}

/// One object or content blob a manifest accounts for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryRecord {
    pub id: [u8; 32],
    pub bytes: u64,
}

/// A native repository snapshot's manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotManifestRecord {
    pub head_generation: u64,
    pub head_envelope: [u8; 32],
    pub envelope: [u8; 32],
    pub objects: Vec<InventoryRecord>,
    pub contents: Vec<InventoryRecord>,
}

/// The fields of one materialization progress record, which its checksum covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationProgressFields {
    pub opcode: u16,
    pub transaction: [u8; 16],
    pub index: u64,
}

fn digest(bytes: [u8; 32]) -> wire::Digest256 {
    wire::Digest256 {
        bytes: bytes.to_vec(),
    }
}
fn digest_array(value: wire::Digest256) -> Result<[u8; 32], StoreRecordError> {
    value
        .bytes
        .try_into()
        .map_err(|_| StoreRecordError::Decode("digest is not 32 bytes".into()))
}
fn encoded<E: std::fmt::Debug>(result: Result<Vec<u8>, E>) -> Result<Vec<u8>, StoreRecordError> {
    result.map_err(|e| StoreRecordError::Decode(format!("{e:?}")))
}
fn decode_error<E: std::fmt::Debug>(error: E) -> StoreRecordError {
    StoreRecordError::Decode(format!("{error:?}"))
}
fn checked<T, E: std::fmt::Debug>(
    bytes: &[u8],
    value: T,
    reencoded: Result<Vec<u8>, E>,
) -> Result<T, StoreRecordError> {
    if encoded(reencoded)? != bytes {
        return Err(StoreRecordError::NonCanonicalEncoding);
    }
    Ok(value)
}

fn uuid(bytes: [u8; 16]) -> wire::Uuid128 {
    wire::Uuid128 {
        bytes: bytes.to_vec(),
    }
}
fn uuid_array(value: wire::Uuid128) -> Result<[u8; 16], StoreRecordError> {
    value
        .bytes
        .try_into()
        .map_err(|_| StoreRecordError::Decode("UUID is not 16 bytes".into()))
}

/// BLAKE3 of `input` after `tag`, cut to its first 16 bytes.
fn digest16(tag: &[u8], input: Result<Vec<u8>, impl std::fmt::Debug>) -> [u8; 16] {
    // Every input below is fixed-size apart from a `uint32`-counted principal name,
    // which `publish_key_digest` bounds, so encoding cannot fail.
    let input = input.expect("a publish digest input encodes");
    let mut hash = blake3::Hasher::new();
    hash.update(tag);
    hash.update(&input);
    let mut out = [0; 16];
    out.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    out
}

/// A client's publish key bound to the principal that sent it.
pub fn publish_key_digest(principal: &[u8], key: [u8; 16]) -> Result<[u8; 16], StoreRecordError> {
    fits_u32_count(principal.len())?;
    Ok(digest16(
        b"wheat-publish-key-v1\0",
        wire::PublishKeyInputV1 {
            principal: principal.to_vec(),
            key: uuid(key),
        }
        .encode(),
    ))
}

/// What a publish asks for: its envelope, on the head it expects, forced or not.
pub fn publish_request_digest(
    envelope: RepositoryEnvelopeId,
    expected_generation: u64,
    expected_envelope: RepositoryEnvelopeId,
    force: bool,
) -> [u8; 16] {
    digest16(
        b"wheat-publish-request-v1\0",
        wire::PublishRequestInputV1 {
            envelope: digest(*envelope.as_bytes()),
            expected_generation,
            expected_envelope: digest(*expected_envelope.as_bytes()),
            force,
        }
        .encode(),
    )
}

/// The receipt ID of a publish, by its key and request digests, published in the
/// head at `generation` with `envelope`.
pub fn publish_receipt_id(
    key: [u8; 16],
    request: [u8; 16],
    generation: u64,
    envelope: RepositoryEnvelopeId,
) -> [u8; 16] {
    digest16(
        b"wheat-publish-receipt-v1\0",
        wire::PublishReceiptIdInputV1 {
            key: uuid(key),
            request: uuid(request),
            generation,
            envelope: digest(*envelope.as_bytes()),
        }
        .encode(),
    )
}

/// Refuses more than [`MAX_PUBLISH_RECEIPTS`] receipts, which also keeps the count
/// within the `uint16` the generated encoder would silently truncate it to.
pub fn encode_repository_head(head: &RepositoryHeadRecord) -> Result<Vec<u8>, StoreRecordError> {
    if head.receipts.len() > MAX_PUBLISH_RECEIPTS {
        return Err(StoreRecordError::InvalidCount);
    }
    let receipts = head
        .receipts
        .iter()
        .map(|receipt| wire::PublishReceiptV1 {
            key: uuid(receipt.key),
            request: uuid(receipt.request),
            receipt: uuid(receipt.receipt),
            generation: receipt.generation,
            envelope: digest(*receipt.envelope.as_bytes()),
            recorded_at: receipt.recorded_at,
        })
        .collect();
    encoded(
        wire::RepositoryHeadV1Input {
            generation: head.generation,
            envelope: digest(*head.envelope.as_bytes()),
            receipts,
        }
        .encode(),
    )
}
pub fn decode_repository_head(bytes: &[u8]) -> Result<RepositoryHeadRecord, StoreRecordError> {
    let receipt_bytes = bytes
        .len()
        .checked_sub(REPOSITORY_HEAD_ENCODED_SIZE)
        .ok_or(StoreRecordError::NonCanonicalEncoding)?;
    if !receipt_bytes.is_multiple_of(PUBLISH_RECEIPT_ENCODED_SIZE) {
        return Err(StoreRecordError::NonCanonicalEncoding);
    }
    if receipt_bytes / PUBLISH_RECEIPT_ENCODED_SIZE > MAX_PUBLISH_RECEIPTS {
        return Err(StoreRecordError::InvalidCount);
    }
    let w = wire::RepositoryHeadV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let receipts = w
        .receipts
        .into_iter()
        .map(|receipt| {
            Ok(PublishReceiptRecord {
                key: uuid_array(receipt.key)?,
                request: uuid_array(receipt.request)?,
                receipt: uuid_array(receipt.receipt)?,
                generation: receipt.generation,
                envelope: RepositoryEnvelopeId::from_digest(digest_array(receipt.envelope)?),
                recorded_at: receipt.recorded_at,
            })
        })
        .collect::<Result<_, StoreRecordError>>()?;
    let head = RepositoryHeadRecord {
        generation: w.generation,
        envelope: RepositoryEnvelopeId::from_digest(digest_array(w.envelope)?),
        receipts,
    };
    checked(bytes, head, reencoded)
}

pub fn encode_content_envelope_header(
    header: &ContentEnvelopeHeaderRecord,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::ContentEnvelopeHeaderV1Input {
            codec: header.codec,
            stored_length: header.stored_length,
            logical_length: header.logical_length,
        }
        .encode(),
    )
}
/// Decodes exactly one header; the codec number is the caller's to check.
pub fn decode_content_envelope_header(
    bytes: &[u8],
) -> Result<ContentEnvelopeHeaderRecord, StoreRecordError> {
    if bytes.len() != CONTENT_ENVELOPE_HEADER_ENCODED_SIZE {
        return Err(StoreRecordError::NonCanonicalEncoding);
    }
    let w = wire::ContentEnvelopeHeaderV1::decode(bytes).map_err(decode_error)?;
    let header = ContentEnvelopeHeaderRecord {
        codec: w.codec,
        stored_length: w.stored_length,
        logical_length: w.logical_length,
    };
    checked(bytes, header, w.encode())
}

/// The generated encoder writes a `uint32` count as `len() as u32`, so a longer
/// array must be refused here rather than silently truncated.
fn inventory_wire(
    entries: impl ExactSizeIterator<Item = InventoryRecord>,
) -> Result<Vec<wire::InventoryEntryV1>, StoreRecordError> {
    fits_u32_count(entries.len())?;
    Ok(entries
        .map(|entry| wire::InventoryEntryV1 {
            id: digest(entry.id),
            bytes: entry.bytes,
        })
        .collect())
}
fn fits_u32_count(length: usize) -> Result<(), StoreRecordError> {
    u32::try_from(length)
        .map(|_| ())
        .map_err(|_| StoreRecordError::InvalidCount)
}
fn inventory_domain(
    entries: Vec<wire::InventoryEntryV1>,
) -> Result<Vec<InventoryRecord>, StoreRecordError> {
    entries
        .into_iter()
        .map(|entry| {
            Ok(InventoryRecord {
                id: digest_array(entry.id)?,
                bytes: entry.bytes,
            })
        })
        .collect()
}

/// Encodes a snapshot manifest from its fields; the inventories are taken as
/// iterators so a caller's own entry type needs no intermediate copy.
pub fn encode_snapshot_manifest(
    head_generation: u64,
    head_envelope: [u8; 32],
    envelope: [u8; 32],
    objects: impl ExactSizeIterator<Item = InventoryRecord>,
    contents: impl ExactSizeIterator<Item = InventoryRecord>,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::SnapshotManifestV1Input {
            head_generation,
            head_envelope: digest(head_envelope),
            envelope: digest(envelope),
            objects: inventory_wire(objects)?,
            contents: inventory_wire(contents)?,
        }
        .encode(),
    )
}
/// Decodes a snapshot manifest holding at most `max_objects` objects and
/// `max_contents` contents. The generated decoder reserves no more entries
/// than the bytes left can hold, so a forged count costs at most the input.
pub fn decode_snapshot_manifest(
    bytes: &[u8],
    max_objects: usize,
    max_contents: usize,
) -> Result<SnapshotManifestRecord, StoreRecordError> {
    let w = wire::SnapshotManifestV1::decode(bytes).map_err(decode_error)?;
    if w.objects.len() > max_objects || w.contents.len() > max_contents {
        return Err(StoreRecordError::InvalidCount);
    }
    let reencoded = w.encode();
    let manifest = SnapshotManifestRecord {
        head_generation: w.head_generation,
        head_envelope: digest_array(w.head_envelope)?,
        envelope: digest_array(w.envelope)?,
        objects: inventory_domain(w.objects)?,
        contents: inventory_domain(w.contents)?,
    };
    checked(bytes, manifest, reencoded)
}

/// The canonical encoding of what one owner keeps alive; it is only ever hashed.
pub fn encode_ownership_manifest(
    owner_kind: u8,
    owner_id: [u8; 32],
    objects: impl ExactSizeIterator<Item = InventoryRecord>,
    contents: impl ExactSizeIterator<Item = InventoryRecord>,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::OwnershipManifestV1Input {
            owner_kind,
            owner_id: digest(owner_id),
            objects: inventory_wire(objects)?,
            contents: inventory_wire(contents)?,
        }
        .encode(),
    )
}

fn progress_fields_wire(
    fields: &MaterializationProgressFields,
) -> wire::MaterializationProgressFieldsV1Input {
    wire::MaterializationProgressFieldsV1Input {
        opcode: fields.opcode,
        transaction: wire::Uuid128 {
            bytes: fields.transaction.to_vec(),
        },
        index: fields.index,
    }
}
/// The encoded fields alone, which the record's checksum covers.
pub fn encode_materialization_progress_fields(
    fields: &MaterializationProgressFields,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(progress_fields_wire(fields).encode())
}
pub fn encode_materialization_progress_record(
    fields: &MaterializationProgressFields,
    checksum: u32,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::MaterializationProgressRecordV1 {
            fields: progress_fields_wire(fields).into(),
            checksum,
        }
        .encode(),
    )
}
/// Decodes one progress record to its fields and checksum; checking the
/// checksum is the caller's.
pub fn decode_materialization_progress_record(
    bytes: &[u8],
) -> Result<(MaterializationProgressFields, u32), StoreRecordError> {
    if bytes.len() != MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE {
        return Err(StoreRecordError::NonCanonicalEncoding);
    }
    let w = wire::MaterializationProgressRecordV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let fields = MaterializationProgressFields {
        opcode: w.fields.opcode,
        transaction: w
            .fields
            .transaction
            .bytes
            .try_into()
            .map_err(|_| StoreRecordError::Decode("transaction is not 16 bytes".into()))?,
        index: w.fields.index,
    };
    checked(bytes, (fields, w.checksum), reencoded)
}

/// The bytes a Git import's journal is bound to: the source path, its `HEAD`
/// file, and every pinned ref with its commit.
pub fn encode_git_import_source<'a>(
    path: &[u8],
    head: &[u8],
    refs: impl ExactSizeIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<Vec<u8>, StoreRecordError> {
    fits_u32_count(path.len())?;
    fits_u32_count(head.len())?;
    fits_u32_count(refs.len())?;
    let refs = refs
        .map(|(name, oid)| {
            fits_u32_count(name.len())?;
            fits_u32_count(oid.len())?;
            Ok(wire::GitImportRefV1 {
                name: name.to_vec(),
                oid: oid.to_vec(),
            })
        })
        .collect::<Result<_, StoreRecordError>>()?;
    encoded(
        wire::GitImportSourceV1Input {
            path: path.to_vec(),
            head: head.to_vec(),
            refs,
        }
        .encode(),
    )
}
