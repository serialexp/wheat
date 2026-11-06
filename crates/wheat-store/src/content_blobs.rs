//! Content kept in an embedder's own blob storage.
//!
//! A [`ContentBlobStore`] holds one record per [`ContentId`]: the bytes Wheat encoded
//! for it, which may be compressed or a delta against other content. The store only
//! keeps bytes under IDs. Encoding, delta chains and checking content against its ID
//! stay in Wheat, so nothing the store returns is trusted without that check.
//! [`ContentStore::over_blobs`] turns a blob store into a [`ContentStore`], which
//! [`crate::SeparateRepositoryStore`] pairs with any object store.

use std::{collections::HashSet, fmt::Debug, future::Future, io, ops::Range, sync::Arc};

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt, stream};
use wheat_core::{ContentId, VerifiedContent};

use crate::{
    ContentStore, ContentStoreError, codec,
    content_store::{ContentBackend, StoredContent, codec_error},
};

/// Writes and reads issued at once by one content operation.
const MAX_CONCURRENT_REQUESTS: usize = 16;

/// Bytes read from a stored record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRead {
    /// The bytes asked for, which end early when the record is shorter.
    pub bytes: Vec<u8>,
    /// The whole record's size in bytes.
    pub size: u64,
}

/// One page of stored IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlobPage {
    pub ids: Vec<ContentId>,
    /// Where the next page starts, or `None` after the last. Only the store that
    /// returned it reads it.
    pub next: Option<String>,
}

/// Blob storage that keeps Wheat's content records, one per [`ContentId`].
///
/// Writing an ID that is already stored replaces its record: the same content can
/// be stored whole or as a delta, so two records for one ID need not be equal.
/// Errors are the store's own, as [`io::Error`]s.
#[async_trait]
pub trait ContentBlobStore: Debug + Send + Sync {
    /// Stores `record` as `id`'s record.
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()>;

    /// `id`'s whole record, or `None` if nothing is stored for it.
    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>>;

    /// The bytes of `id`'s record in `range`, or `None` if nothing is stored for it.
    /// Reading an envelope's length uses this, so it should not fetch the rest.
    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>>;

    /// Up to `limit` stored IDs, starting where `cursor` says (the first page when
    /// `None`); or `None` if this store does not list. A store that lists answers
    /// [`ContentStore::existing_ids`] from the listing when that takes fewer requests
    /// than one read per ID.
    async fn list_page(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> io::Result<Option<BlobPage>> {
        let _ = (cursor, limit);
        Ok(None)
    }
}

impl ContentStore {
    /// A content store whose records are kept in `blobs`.
    ///
    /// Reads and writes block the calling thread on the current Tokio runtime, which
    /// must be multi-threaded.
    #[must_use]
    pub fn over_blobs(blobs: Arc<dyn ContentBlobStore>) -> Self {
        // No loose files: the blob backend handles every read and write.
        Self::with_backend(
            std::path::PathBuf::from("/dev/null"),
            Arc::new(BlobBackend { blobs }),
            crate::Durability::Durable,
        )
    }
}

/// IDs asked for per listed page.
const LIST_PAGE: usize = 1000;

/// A [`ContentBlobStore`] as a content backend.
#[derive(Debug)]
struct BlobBackend {
    blobs: Arc<dyn ContentBlobStore>,
}

/// Runs `future` to completion from the synchronous backend calls.
fn block_on<F: Future>(future: F) -> Result<F::Output, ContentStoreError> {
    let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    Ok(tokio::task::block_in_place(|| runtime.block_on(future)))
}

fn malformed(id: ContentId, reason: &'static str) -> ContentStoreError {
    codec_error(id, usize::MAX, codec::CodecError::Format(reason))
}

/// Parses the header of `id`'s record, which is `size` bytes long in all.
/// The logical length of `id`'s stored `record`, as [`crate::ContentStore::logical_length`]
/// answers it over a blob store, from the record in hand: the whole blob's length for a
/// chunked one's manifest, the reconstructed length for a delta. Only the header is
/// read; it is checked against the record's length and its codec's bounds, but the
/// payload is not checked against it, as reading the content does.
pub fn record_logical_length(id: ContentId, record: &[u8]) -> Result<u64, ContentStoreError> {
    Ok(record_info(id, record)?.logical_length)
}

/// The other content a stored record needs to be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordReferences {
    /// It is stored whole.
    None,
    /// It is a delta against `base`, which must be kept while it is.
    Delta { base: ContentId },
    /// It is a chunked blob's manifest listing these chunks, in order.
    Chunks(Vec<ContentId>),
    /// It is a long chunked blob's manifest listing these parts, in order. Each part
    /// is an ordinary blob whose content lists more chunks
    /// ([`wheat_core::ContentChunkPartRecord`]), found only by reading it.
    Parts(Vec<ContentId>),
}

/// What a stored record says about itself, from the record alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordInfo {
    /// As [`record_logical_length`].
    pub logical_length: u64,
    pub references: RecordReferences,
}

/// The logical length of `id`'s stored `record` and the content it refers to, for an
/// embedder to keep with the record when it is put: content GC follows a record's
/// references without reading it. The header is checked as by
/// [`record_logical_length`]; a chunk manifest is decoded and checked whole, and a
/// delta's payload is not decoded beyond its base.
pub fn record_info(id: ContentId, record: &[u8]) -> Result<RecordInfo, ContentStoreError> {
    let header = checked_header(
        id,
        record.get(..codec::ENVELOPE_HEADER_LEN).unwrap_or(record),
        record.len() as u64,
    )?;
    header
        .validate(record.len() as u64, usize::MAX)
        .map_err(|error| codec_error(id, usize::MAX, error))?;
    let payload = &record[codec::ENVELOPE_HEADER_LEN..];
    Ok(RecordInfo {
        logical_length: header.logical_length,
        references: payload_references(id, &header, payload)?,
    })
}

/// What a record whose header is `header` refers to, from its `payload`: a delta's
/// leading base reference (the rest may be left out), or a chunked blob's whole
/// manifest.
pub(crate) fn payload_references(
    id: ContentId,
    header: &codec::EnvelopeHeader,
    payload: &[u8],
) -> Result<RecordReferences, ContentStoreError> {
    Ok(match header.codec {
        codec::Codec::Raw | codec::Codec::Zstd => RecordReferences::None,
        codec::Codec::Delta | codec::Codec::DeltaZstd => {
            let base: [u8; 32] = payload
                .get(..crate::delta::BASE_REFERENCE_SIZE)
                .and_then(|base| base.try_into().ok())
                .ok_or_else(|| malformed(id, "delta payload too short for base reference"))?;
            RecordReferences::Delta {
                base: ContentId::from_digest(base),
            }
        }
        codec::Codec::Chunked => {
            let manifest = wheat_core::decode_content_chunk_manifest(payload)
                .map_err(|_| malformed(id, "invalid chunk manifest"))?;
            if manifest.logical_length != header.logical_length {
                return Err(malformed(id, "chunk manifest length differs"));
            }
            match manifest.entries {
                wheat_core::ContentManifestEntries::Chunks(chunks) => {
                    RecordReferences::Chunks(chunks.into_iter().map(|chunk| chunk.id).collect())
                }
                wheat_core::ContentManifestEntries::Parts(parts) => {
                    RecordReferences::Parts(parts.into_iter().map(|part| part.id).collect())
                }
            }
        }
        codec::Codec::ZstdBlock => {
            return Err(malformed(
                id,
                "a block-compressed record is not stored alone",
            ));
        }
    })
}

fn checked_header(
    id: ContentId,
    header: &[u8],
    size: u64,
) -> Result<codec::EnvelopeHeader, ContentStoreError> {
    if header.len() != codec::ENVELOPE_HEADER_LEN {
        return Err(malformed(id, "bad envelope prefix"));
    }
    let parsed =
        codec::parse_envelope_header(header).map_err(|error| codec_error(id, usize::MAX, error))?;
    // Only the physical length is checked here. A delta's logical length differs
    // from what it stores, and the caller's bound applies after reconstruction.
    if parsed
        .physical_length()
        .map_err(|error| codec_error(id, usize::MAX, error))?
        != size
    {
        return Err(malformed(id, "stored length differs"));
    }
    Ok(parsed)
}

impl BlobBackend {
    /// Every stored ID, if listing them takes at most `pages` requests; `None` if it
    /// takes more, or the store does not list.
    async fn list_within(&self, pages: usize) -> io::Result<Option<HashSet<ContentId>>> {
        let mut ids = HashSet::new();
        let mut cursor = None;
        for _ in 0..pages {
            let Some(page) = self.blobs.list_page(cursor, LIST_PAGE).await? else {
                return Ok(None);
            };
            ids.extend(page.ids);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(Some(ids)),
            }
        }
        Ok(None)
    }
}

impl ContentBackend for BlobBackend {
    fn put_many(&self, contents: &[VerifiedContent<&[u8]>]) -> Result<(), ContentStoreError> {
        block_on(async {
            // Encoding inside the stream bounds the envelopes held while the store
            // is slower than compression.
            stream::iter(contents.iter().map(|content| async move {
                let id = content.id();
                let record = codec::encode_envelope(content.bytes())
                    .map_err(|error| codec_error(id, usize::MAX, error))?;
                self.blobs
                    .put(id, record)
                    .await
                    .map_err(ContentStoreError::Io)
            }))
            .buffer_unordered(MAX_CONCURRENT_REQUESTS)
            .try_collect::<()>()
            .await
        })?
    }

    fn get_stored(&self, id: ContentId) -> Result<StoredContent, ContentStoreError> {
        let mut record = block_on(self.blobs.get(id))??.ok_or(ContentStoreError::Missing(id))?;
        let header = checked_header(
            id,
            record
                .get(..codec::ENVELOPE_HEADER_LEN)
                .ok_or_else(|| malformed(id, "bad envelope prefix"))?,
            record.len() as u64,
        )?;
        record.drain(..codec::ENVELOPE_HEADER_LEN);
        Ok(StoredContent::Envelope(header, record))
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        let read = block_on(
            self.blobs
                .get_range(id, 0..codec::ENVELOPE_HEADER_LEN as u64),
        )??
        .ok_or(ContentStoreError::Missing(id))?;
        checked_header(id, &read.bytes, read.size).map(|header| header.logical_length)
    }

    fn existing_ids(
        &self,
        candidates: &[ContentId],
    ) -> Option<Result<HashSet<ContentId>, ContentStoreError>> {
        let found =
            block_on(async {
                // A listing costs one request per LIST_PAGE stored blobs, one after
                // another; checking costs one request per candidate, a window of them at
                // once. Only as many pages are listed as checking would take rounds, so
                // neither a large store nor a large check pays more than twice the cheaper.
                let rounds = candidates.len().div_ceil(MAX_CONCURRENT_REQUESTS);
                if let Some(listed) = self.list_within(rounds).await? {
                    return Ok(candidates
                        .iter()
                        .copied()
                        .filter(|id| listed.contains(id))
                        .collect());
                }
                // Present as `ContentStore::contains` has it: a record whose header reads.
                stream::iter(candidates.iter().map(|&id| async move {
                    let read = self
                        .blobs
                        .get_range(id, 0..codec::ENVELOPE_HEADER_LEN as u64)
                        .await?;
                    Ok::<_, io::Error>(read.and_then(|read| {
                        checked_header(id, &read.bytes, read.size).ok().map(|_| id)
                    }))
                }))
                .buffer_unordered(MAX_CONCURRENT_REQUESTS)
                .try_filter_map(|id| async move { Ok(id) })
                .try_collect::<HashSet<_>>()
                .await
            });
        Some(match found {
            Ok(Ok(found)) => Ok(found),
            Ok(Err(error)) => Err(ContentStoreError::Io(error)),
            Err(error) => Err(error),
        })
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        // Each envelope is copied as its write starts, so at most a window of
        // copies is held at once.
        block_on(
            stream::iter(envelopes)
                .map(|(id, envelope)| self.blobs.put(*id, envelope.clone()))
                .buffer_unordered(MAX_CONCURRENT_REQUESTS)
                .try_collect::<()>(),
        )?
        .map_err(ContentStoreError::Io)
    }

    fn supports_raw_envelopes(&self) -> bool {
        true
    }

    fn references_many(
        &self,
        ids: &[ContentId],
    ) -> Option<Result<Vec<RecordReferences>, ContentStoreError>> {
        // The header and a delta's base reference in one ranged read; only a chunked
        // blob's manifest is read whole.
        let prefix = (codec::ENVELOPE_HEADER_LEN + crate::delta::BASE_REFERENCE_SIZE) as u64;
        let found = block_on(
            stream::iter(ids.iter().map(|&id| async move {
                let read = self
                    .blobs
                    .get_range(id, 0..prefix)
                    .await?
                    .ok_or(ContentStoreError::Missing(id))?;
                let header = checked_header(
                    id,
                    read.bytes
                        .get(..codec::ENVELOPE_HEADER_LEN)
                        .ok_or_else(|| malformed(id, "bad envelope prefix"))?,
                    read.size,
                )?;
                if header.codec == codec::Codec::Chunked {
                    let record = self
                        .blobs
                        .get(id)
                        .await?
                        .ok_or(ContentStoreError::Missing(id))?;
                    return Ok(record_info(id, &record)?.references);
                }
                payload_references(id, &header, &read.bytes[codec::ENVELOPE_HEADER_LEN..])
            }))
            .buffered(MAX_CONCURRENT_REQUESTS)
            .try_collect::<Vec<_>>(),
        );
        Some(found.and_then(|found| found))
    }
}
