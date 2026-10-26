//! Content kept in an embedder's own blob storage.
//!
//! A [`ContentBlobStore`] holds one record per [`ContentId`]: the bytes Wheat encoded
//! for it, which may be compressed or a delta against other content. The store only
//! keeps bytes under IDs. Encoding, delta chains and checking content against its ID
//! stay in Wheat, so nothing the store returns is trusted without that check.
//! [`ContentStore::over_blobs`] turns a blob store into a [`ContentStore`], which
//! [`crate::SeparateRepositoryStore`] pairs with any object store.

use std::{fmt::Debug, future::Future, io, ops::Range, sync::Arc};

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
    /// [`ContentStore::existing_ids`] from the listing instead of one read per ID.
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

    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        let listed = block_on(async {
            let mut ids = Vec::new();
            let mut cursor = None;
            loop {
                let Some(page) = self.blobs.list_page(cursor, LIST_PAGE).await? else {
                    return Ok::<_, io::Error>(None);
                };
                ids.extend(page.ids);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => return Ok(Some(ids)),
                }
            }
        });
        match listed {
            Ok(Ok(Some(ids))) => Some(Ok(ids)),
            Ok(Ok(None)) => None,
            Ok(Err(error)) => Some(Err(ContentStoreError::Io(error))),
            Err(error) => Some(Err(error)),
        }
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
}
