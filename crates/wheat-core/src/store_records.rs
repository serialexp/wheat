//! Safe domain wrappers for the stores' small persisted records: the repository
//! head, the content envelope header, snapshot and ownership manifests, a
//! workspace materialization's progress log, a Git import's source binding, a
//! served repository's location, a chunked blob's manifest, and the control files a
//! client keeps: store and workspace metadata, workspace pins and the remote binding.
//!
//! Like the physical segment records, none of these are `WheatObject` variants.

use thiserror::Error;
use uuid::Uuid;
use wheat_binschema::generated as wire;

use crate::{ChangeId, ContentId, RepositoryEnvelopeId, TreeId, ViewId};

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
/// A blob longer than this is stored as content-defined chunks and a manifest; one
/// this long or shorter is stored whole.
pub const CHUNKED_CONTENT_THRESHOLD: u64 = 16 << 20;
/// The longest blob a chunk manifest describes.
pub const MAX_CHUNKED_CONTENT_BYTES: u64 = 1 << 40;
/// The shortest chunk but a blob's last.
pub const MIN_CONTENT_CHUNK_BYTES: u32 = 1 << 20;
/// The longest chunk.
pub const MAX_CONTENT_CHUNK_BYTES: u32 = 4 << 20;
/// The most chunks a manifest lists: the longest blob cut at the shortest chunks.
pub const MAX_CONTENT_CHUNKS: usize =
    (MAX_CHUNKED_CONTENT_BYTES / MIN_CONTENT_CHUNK_BYTES as u64) as usize;
/// Encoded size of a chunk manifest with no chunks.
pub const CONTENT_CHUNK_MANIFEST_FIXED_ENCODED_SIZE: usize = 17;
/// Encoded size of one chunk in a chunk manifest.
pub const CONTENT_CHUNK_ENCODED_SIZE: usize = 36;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum StoreRecordError {
    #[error("store record does not decode: {0}")]
    Decode(String),
    #[error("store record is not exactly and canonically encoded")]
    NonCanonicalEncoding,
    #[error("store record count exceeds its limit or the supplied bytes")]
    InvalidCount,
    #[error("store record's {0} is the nil UUID")]
    NilIdentity(&'static str),
    #[error("chunk manifest is invalid: {0}")]
    InvalidChunkManifest(&'static str),
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

/// One chunk of a chunked blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentChunkRecord {
    pub id: ContentId,
    pub length: u32,
}

/// What a blob over [`CHUNKED_CONTENT_THRESHOLD`] is stored as: its chunks, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentChunkManifestRecord {
    /// The blob's length; the chunks' lengths sum to it.
    pub logical_length: u64,
    pub chunks: Vec<ContentChunkRecord>,
}

impl ContentChunkManifestRecord {
    /// Checks the manifest describes a blob that is chunked, within the format's size,
    /// cut into chunks the chunker could produce. It cannot check the chunks are cut
    /// where the chunker would cut them; a blob's identity is its bytes, so a manifest
    /// cut elsewhere names the same content.
    pub fn validate(&self) -> Result<(), StoreRecordError> {
        if self.logical_length <= CHUNKED_CONTENT_THRESHOLD {
            return Err(StoreRecordError::InvalidChunkManifest(
                "the blob is short enough to store whole",
            ));
        }
        if self.logical_length > MAX_CHUNKED_CONTENT_BYTES {
            return Err(StoreRecordError::InvalidChunkManifest(
                "the blob is longer than the format allows",
            ));
        }
        if self.chunks.len() > MAX_CONTENT_CHUNKS {
            return Err(StoreRecordError::InvalidCount);
        }
        let Some((last, rest)) = self.chunks.split_last() else {
            return Err(StoreRecordError::InvalidChunkManifest("it has no chunks"));
        };
        if rest.iter().any(|chunk| {
            !(MIN_CONTENT_CHUNK_BYTES..=MAX_CONTENT_CHUNK_BYTES).contains(&chunk.length)
        }) {
            return Err(StoreRecordError::InvalidChunkManifest(
                "a chunk before the last is outside the chunk sizes",
            ));
        }
        if !(1..=MAX_CONTENT_CHUNK_BYTES).contains(&last.length) {
            return Err(StoreRecordError::InvalidChunkManifest(
                "the last chunk is empty or too long",
            ));
        }
        // At most `MAX_CONTENT_CHUNKS` chunks of at most 4 MiB: no overflow.
        let total: u64 = self
            .chunks
            .iter()
            .map(|chunk| u64::from(chunk.length))
            .sum();
        if total != self.logical_length {
            return Err(StoreRecordError::InvalidChunkManifest(
                "the chunks' lengths do not sum to the blob's",
            ));
        }
        Ok(())
    }
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

/// Where a server keeps a repository: the fork network whose store holds its objects,
/// and its identity in that network, which names its head. Neither is nil.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RepositoryLocationRecord {
    pub network: Uuid,
    pub repository: Uuid,
}

/// `store.meta`: a store's identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreMetadataRecord {
    pub store_id: Uuid,
}

/// A repository head a workspace holds on to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryRootRecord {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
}

/// The head, view and tree a workspace's files were (or are being) written from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedRootsRecord {
    pub repository_head: RepositoryRootRecord,
    pub view: ViewId,
    pub tree: TreeId,
}

/// A repository compare-and-swap a workspace has in flight, and the frontier it
/// leaves the workspace on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingRepositoryCasRecord {
    pub expected_head: RepositoryRootRecord,
    pub proposed_head: RepositoryRootRecord,
    pub resulting_frontier: ViewId,
}

/// `.wheat/workspace.meta`: a workspace's binding to its store and label. Neither
/// identity is nil.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMetadataRecord {
    pub workspace_id: Uuid,
    pub registration_token: Uuid,
    pub store_path: String,
    pub store_id: Uuid,
    pub active_label: String,
    pub basis: ViewId,
    pub observed_label_binding: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub materialized: Option<MaterializedRootsRecord>,
    pub selected_change: Option<ChangeId>,
    pub pending_repository_operation: Option<PendingRepositoryOperationRecord>,
}

/// A workspace's repository compare-and-swap in flight, and how it leaves the workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingRepositoryOperationRecord {
    pub cas: PendingRepositoryCasRecord,
    /// The change the workspace has selected afterwards.
    pub selected_change: Option<ChangeId>,
    /// The operation moves the workspace's label to the resulting frontier, so the
    /// workspace rebinds there.
    pub rebinds: bool,
}

/// `workspaces/<workspace id>.pin` in a store: the roots a workspace keeps alive.
/// No identity is nil.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePinRecord {
    pub store_id: Uuid,
    pub workspace_id: Uuid,
    pub registration_token: Uuid,
    pub workspace_path: String,
    pub basis: ViewId,
    pub observed_label_binding: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub selected_change: Option<ChangeId>,
    pub materialized: Option<MaterializedRootsRecord>,
    pub pending_materialization: Option<MaterializedRootsRecord>,
    pub pending_repository_cas: Option<PendingRepositoryCasRecord>,
}

/// `remote.meta` in a store: the remote it talks to and the remote head it last
/// observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteConfigRecord {
    pub display_url: String,
    pub canonical_identity: String,
    pub head: RepositoryRootRecord,
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

/// What a publish asks for: its envelope, on the head it expects, forced or not, and
/// whether it may move labels backward.
pub fn publish_request_digest(
    envelope: RepositoryEnvelopeId,
    expected_generation: u64,
    expected_envelope: RepositoryEnvelopeId,
    force: bool,
    force_labels: bool,
) -> [u8; 16] {
    digest16(
        b"wheat-publish-request-v1\0",
        wire::PublishRequestInputV1 {
            envelope: digest(*envelope.as_bytes()),
            expected_generation,
            expected_envelope: digest(*expected_envelope.as_bytes()),
            force,
            force_labels,
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

/// Encodes a valid chunk manifest.
pub fn encode_content_chunk_manifest(
    manifest: &ContentChunkManifestRecord,
) -> Result<Vec<u8>, StoreRecordError> {
    manifest.validate()?;
    encoded(
        wire::ContentChunkManifestV1Input {
            logical_length: manifest.logical_length,
            chunks: manifest
                .chunks
                .iter()
                .map(|chunk| wire::ContentChunkRefV1 {
                    id: wire::ContentId {
                        digest: digest(*chunk.id.as_bytes()),
                    },
                    length: chunk.length,
                })
                .collect(),
        }
        .encode(),
    )
}
/// Decodes exactly one canonical, valid chunk manifest.
pub fn decode_content_chunk_manifest(
    bytes: &[u8],
) -> Result<ContentChunkManifestRecord, StoreRecordError> {
    let w = wire::ContentChunkManifestV1::decode(bytes).map_err(decode_error)?;
    if w.chunks.len() > MAX_CONTENT_CHUNKS {
        return Err(StoreRecordError::InvalidCount);
    }
    let reencoded = w.encode();
    let manifest = ContentChunkManifestRecord {
        logical_length: w.logical_length,
        chunks: w
            .chunks
            .into_iter()
            .map(|chunk| {
                Ok(ContentChunkRecord {
                    id: ContentId::from_digest(digest_array(chunk.id.digest)?),
                    length: chunk.length,
                })
            })
            .collect::<Result<_, StoreRecordError>>()?,
    };
    let manifest = checked(bytes, manifest, reencoded)?;
    manifest.validate()?;
    Ok(manifest)
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

/// A file of a bundle's structure; see `BundleFileV1` in the schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleFileRecord {
    /// Relative to the bundle's `structure/` directory, `/`-separated.
    pub path: String,
    pub length: u64,
    pub digest: [u8; 32],
}

/// An exported fork network's manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleManifestRecord {
    pub network: Uuid,
    /// Sorted.
    pub repositories: Vec<Uuid>,
    /// Sorted by path.
    pub files: Vec<BundleFileRecord>,
    pub content_count: u64,
    /// The content's logical bytes.
    pub content_bytes: u64,
    /// The digest of each content list chunk, in order.
    pub content_chunks: Vec<[u8; 32]>,
}

pub fn encode_bundle_manifest(record: &BundleManifestRecord) -> Result<Vec<u8>, StoreRecordError> {
    fits_u32_count(record.repositories.len())?;
    fits_u32_count(record.files.len())?;
    fits_u32_count(record.content_chunks.len())?;
    encoded(
        wire::BundleManifestV1Input {
            network: identity(record.network),
            repositories: record.repositories.iter().copied().map(identity).collect(),
            files: record
                .files
                .iter()
                .map(|file| wire::BundleFileV1 {
                    path: file.path.clone(),
                    length: file.length,
                    digest: digest(file.digest),
                })
                .collect(),
            content_count: record.content_count,
            content_bytes: record.content_bytes,
            content_chunks: record.content_chunks.iter().copied().map(digest).collect(),
        }
        .encode(),
    )
}
pub fn decode_bundle_manifest(bytes: &[u8]) -> Result<BundleManifestRecord, StoreRecordError> {
    let w = wire::BundleManifestV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = BundleManifestRecord {
        network: identity_domain(w.network, "network ID")?,
        repositories: w
            .repositories
            .into_iter()
            .map(|id| identity_domain(id, "repository ID"))
            .collect::<Result<_, _>>()?,
        files: w
            .files
            .into_iter()
            .map(|file| {
                Ok(BundleFileRecord {
                    path: file.path,
                    length: file.length,
                    digest: digest_array(file.digest)?,
                })
            })
            .collect::<Result<_, StoreRecordError>>()?,
        content_count: w.content_count,
        content_bytes: w.content_bytes,
        content_chunks: w
            .content_chunks
            .into_iter()
            .map(digest_array)
            .collect::<Result<_, _>>()?,
    };
    checked(bytes, record, reencoded)
}

/// Encodes one chunk of a bundle's content list.
pub fn encode_bundle_content_chunk(
    contents: impl ExactSizeIterator<Item = InventoryRecord>,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::BundleContentChunkV1Input {
            contents: inventory_wire(contents)?,
        }
        .encode(),
    )
}
pub fn decode_bundle_content_chunk(bytes: &[u8]) -> Result<Vec<InventoryRecord>, StoreRecordError> {
    let w = wire::BundleContentChunkV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let contents = inventory_domain(w.contents)?;
    checked(bytes, contents, reencoded)
}

fn identity(value: Uuid) -> wire::Uuid128 {
    uuid(*value.as_bytes())
}
fn identity_domain(value: wire::Uuid128, field: &'static str) -> Result<Uuid, StoreRecordError> {
    let value = Uuid::from_bytes(uuid_array(value)?);
    if value.is_nil() {
        return Err(StoreRecordError::NilIdentity(field));
    }
    Ok(value)
}
fn view_wire(value: ViewId) -> wire::ViewId {
    wire::ViewId {
        digest: digest(*value.as_bytes()),
    }
}
fn view_domain(value: wire::ViewId) -> Result<ViewId, StoreRecordError> {
    Ok(ViewId::from_digest(digest_array(value.digest)?))
}
fn change_wire(value: ChangeId) -> wire::ChangeId {
    wire::ChangeId {
        uuid: uuid(*value.as_bytes()),
    }
}
fn change_domain(value: wire::ChangeId) -> Result<ChangeId, StoreRecordError> {
    Ok(ChangeId::from_bytes(uuid_array(value.uuid)?))
}
fn root_wire(value: RepositoryRootRecord) -> wire::RepositoryRootV1 {
    wire::RepositoryRootV1 {
        generation: value.generation,
        envelope: digest(*value.envelope.as_bytes()),
    }
}
fn root_domain(value: wire::RepositoryRootV1) -> Result<RepositoryRootRecord, StoreRecordError> {
    Ok(RepositoryRootRecord {
        generation: value.generation,
        envelope: RepositoryEnvelopeId::from_digest(digest_array(value.envelope)?),
    })
}
fn materialized_wire(value: MaterializedRootsRecord) -> wire::MaterializedRootsV1 {
    wire::MaterializedRootsV1 {
        repository_head: root_wire(value.repository_head),
        view: view_wire(value.view),
        tree: wire::TreeId {
            digest: digest(*value.tree.as_bytes()),
        },
    }
}
fn materialized_domain(
    value: wire::MaterializedRootsV1,
) -> Result<MaterializedRootsRecord, StoreRecordError> {
    Ok(MaterializedRootsRecord {
        repository_head: root_domain(value.repository_head)?,
        view: view_domain(value.view)?,
        tree: TreeId::from_digest(digest_array(value.tree.digest)?),
    })
}
fn cas_wire(value: PendingRepositoryCasRecord) -> wire::PendingRepositoryCasV1 {
    wire::PendingRepositoryCasV1 {
        expected_head: root_wire(value.expected_head),
        proposed_head: root_wire(value.proposed_head),
        resulting_frontier: view_wire(value.resulting_frontier),
    }
}
fn cas_domain(
    value: wire::PendingRepositoryCasV1,
) -> Result<PendingRepositoryCasRecord, StoreRecordError> {
    Ok(PendingRepositoryCasRecord {
        expected_head: root_domain(value.expected_head)?,
        proposed_head: root_domain(value.proposed_head)?,
        resulting_frontier: view_domain(value.resulting_frontier)?,
    })
}

pub fn encode_repository_location(
    record: RepositoryLocationRecord,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::RepositoryLocationV1Input {
            network: identity(record.network),
            repository: identity(record.repository),
        }
        .encode(),
    )
}
pub fn decode_repository_location(
    bytes: &[u8],
) -> Result<RepositoryLocationRecord, StoreRecordError> {
    let w = wire::RepositoryLocationV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = RepositoryLocationRecord {
        network: identity_domain(w.network, "network ID")?,
        repository: identity_domain(w.repository, "repository ID")?,
    };
    checked(bytes, record, reencoded)
}

pub fn encode_store_metadata(record: StoreMetadataRecord) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::StoreMetadataV1Input {
            store_id: identity(record.store_id),
        }
        .encode(),
    )
}
pub fn decode_store_metadata(bytes: &[u8]) -> Result<StoreMetadataRecord, StoreRecordError> {
    let w = wire::StoreMetadataV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = StoreMetadataRecord {
        store_id: identity_domain(w.store_id, "store ID")?,
    };
    checked(bytes, record, reencoded)
}

pub fn encode_workspace_metadata(
    record: &WorkspaceMetadataRecord,
) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::WorkspaceMetadataV1Input {
            workspace_id: identity(record.workspace_id),
            registration_token: identity(record.registration_token),
            store_path: record.store_path.clone(),
            store_id: identity(record.store_id),
            active_label: record.active_label.clone(),
            basis: view_wire(record.basis),
            observed_label_binding: view_wire(record.observed_label_binding),
            selected_frontier: record.selected_frontier.map(view_wire),
            materialized: record.materialized.map(materialized_wire),
            selected_change: record.selected_change.map(change_wire),
            pending_repository_operation: record.pending_repository_operation.map(|operation| {
                wire::PendingRepositoryOperationV1 {
                    cas: cas_wire(operation.cas),
                    selected_change: operation.selected_change.map(change_wire),
                    rebinds: operation.rebinds,
                }
            }),
        }
        .encode(),
    )
}
pub fn decode_workspace_metadata(
    bytes: &[u8],
) -> Result<WorkspaceMetadataRecord, StoreRecordError> {
    let w = wire::WorkspaceMetadataV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = WorkspaceMetadataRecord {
        workspace_id: identity_domain(w.workspace_id, "workspace ID")?,
        registration_token: identity_domain(w.registration_token, "registration token")?,
        store_path: w.store_path,
        store_id: identity_domain(w.store_id, "store ID")?,
        active_label: w.active_label,
        basis: view_domain(w.basis)?,
        observed_label_binding: view_domain(w.observed_label_binding)?,
        selected_frontier: w.selected_frontier.map(view_domain).transpose()?,
        materialized: w.materialized.map(materialized_domain).transpose()?,
        selected_change: w.selected_change.map(change_domain).transpose()?,
        pending_repository_operation: w
            .pending_repository_operation
            .map(|operation| {
                Ok::<_, StoreRecordError>(PendingRepositoryOperationRecord {
                    cas: cas_domain(operation.cas)?,
                    selected_change: operation.selected_change.map(change_domain).transpose()?,
                    rebinds: operation.rebinds,
                })
            })
            .transpose()?,
    };
    checked(bytes, record, reencoded)
}

pub fn encode_workspace_pin(record: &WorkspacePinRecord) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::WorkspacePinV1Input {
            store_id: identity(record.store_id),
            workspace_id: identity(record.workspace_id),
            registration_token: identity(record.registration_token),
            workspace_path: record.workspace_path.clone(),
            basis: view_wire(record.basis),
            observed_label_binding: view_wire(record.observed_label_binding),
            selected_frontier: record.selected_frontier.map(view_wire),
            selected_change: record.selected_change.map(change_wire),
            materialized: record.materialized.map(materialized_wire),
            pending_materialization: record.pending_materialization.map(materialized_wire),
            pending_repository_cas: record.pending_repository_cas.map(cas_wire),
        }
        .encode(),
    )
}
pub fn decode_workspace_pin(bytes: &[u8]) -> Result<WorkspacePinRecord, StoreRecordError> {
    let w = wire::WorkspacePinV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = WorkspacePinRecord {
        store_id: identity_domain(w.store_id, "store ID")?,
        workspace_id: identity_domain(w.workspace_id, "workspace ID")?,
        registration_token: identity_domain(w.registration_token, "registration token")?,
        workspace_path: w.workspace_path,
        basis: view_domain(w.basis)?,
        observed_label_binding: view_domain(w.observed_label_binding)?,
        selected_frontier: w.selected_frontier.map(view_domain).transpose()?,
        selected_change: w.selected_change.map(change_domain).transpose()?,
        materialized: w.materialized.map(materialized_domain).transpose()?,
        pending_materialization: w
            .pending_materialization
            .map(materialized_domain)
            .transpose()?,
        pending_repository_cas: w.pending_repository_cas.map(cas_domain).transpose()?,
    };
    checked(bytes, record, reencoded)
}

pub fn encode_remote_config(record: &RemoteConfigRecord) -> Result<Vec<u8>, StoreRecordError> {
    encoded(
        wire::RemoteConfigV1Input {
            display_url: record.display_url.clone(),
            canonical_identity: record.canonical_identity.clone(),
            head: root_wire(record.head),
        }
        .encode(),
    )
}
pub fn decode_remote_config(bytes: &[u8]) -> Result<RemoteConfigRecord, StoreRecordError> {
    let w = wire::RemoteConfigV1::decode(bytes).map_err(decode_error)?;
    let reencoded = w.encode();
    let record = RemoteConfigRecord {
        display_url: w.display_url,
        canonical_identity: w.canonical_identity,
        head: root_domain(w.head)?,
    };
    checked(bytes, record, reencoded)
}
