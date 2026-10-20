//! Remote content backend — stores content on S3 as individual keys.

use std::sync::Arc;

use futures::{StreamExt, TryStreamExt, stream};
use object_store::{ObjectStore as BackendObjectStore, ObjectStoreExt, path::Path};
use wheat_core::{ContentId, VerifiedContent};

use crate::content_store::ContentBackend;
use crate::{ContentStoreError, codec};

fn codec_error(id: ContentId, max: usize, error: codec::CodecError) -> ContentStoreError {
    match error {
        codec::CodecError::ReadLimit { logical, .. } => ContentStoreError::TooLarge {
            id,
            max,
            minimum: logical,
        },
        codec::CodecError::HardLimit => ContentStoreError::TooLarge {
            id,
            max,
            minimum: codec::HARD_LOGICAL_RECORD_BYTES.saturating_add(1),
        },
        other => ContentStoreError::InvalidFormat {
            id,
            reason: other.to_string(),
        },
    }
}

const MAX_CONCURRENT_REQUESTS: usize = 16;

#[cfg(test)]
async fn fetch_bounded_envelope(
    backend: &dyn BackendObjectStore,
    path: &Path,
    id: ContentId,
    max: usize,
) -> Result<Vec<u8>, ContentStoreError> {
    let result = backend.get(path).await.map_err(|error| match error {
        object_store::Error::NotFound { .. } => ContentStoreError::Missing(id),
        error => ContentStoreError::Io(std::io::Error::other(error)),
    })?;
    let physical_length = result.meta.size;
    if physical_length < codec::ENVELOPE_HEADER_LEN as u64 {
        return Err(codec_error(
            id,
            max,
            codec::CodecError::Format("bad envelope prefix"),
        ));
    }

    let mut stream = result.into_stream();
    let mut header_bytes = [0_u8; codec::ENVELOPE_HEADER_LEN];
    let mut header_filled = 0;
    let mut header = None;
    let mut stored = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
        let mut chunk = chunk.as_ref();
        if header_filled < codec::ENVELOPE_HEADER_LEN {
            let take = (codec::ENVELOPE_HEADER_LEN - header_filled).min(chunk.len());
            header_bytes[header_filled..header_filled + take].copy_from_slice(&chunk[..take]);
            header_filled += take;
            chunk = &chunk[take..];
            if header_filled == codec::ENVELOPE_HEADER_LEN {
                let parsed = codec::parse_envelope_header(&header_bytes)
                    .map_err(|error| codec_error(id, max, error))?;
                parsed
                    .validate(physical_length, max)
                    .map_err(|error| codec_error(id, max, error))?;
                let capacity = usize::try_from(parsed.stored_length).map_err(|_| {
                    codec_error(
                        id,
                        max,
                        codec::CodecError::Format("stored length does not fit this platform"),
                    )
                })?;
                stored = Some(Vec::with_capacity(capacity));
                header = Some(parsed);
            }
        }
        if !chunk.is_empty() {
            let payload = stored.as_mut().ok_or_else(|| {
                codec_error(id, max, codec::CodecError::Format("bad envelope prefix"))
            })?;
            let expected = header
                .expect("payload allocation follows parsed header")
                .stored_length;
            if payload.len().saturating_add(chunk.len()) > expected as usize {
                return Err(codec_error(
                    id,
                    max,
                    codec::CodecError::Format("stored length differs"),
                ));
            }
            payload.extend_from_slice(chunk);
        }
    }
    let header = header
        .ok_or_else(|| codec_error(id, max, codec::CodecError::Format("bad envelope prefix")))?;
    let payload = stored.expect("parsed header allocates payload");
    if payload.len() as u64 != header.stored_length {
        return Err(codec_error(
            id,
            max,
            codec::CodecError::Format("stored length differs"),
        ));
    }
    let bytes = codec::decode_envelope_payload(header, payload, max)
        .map_err(|error| codec_error(id, max, error))?;
    let actual = ContentId::of(&bytes);
    if actual != id {
        return Err(ContentStoreError::Corrupt {
            expected: id,
            actual,
        });
    }
    Ok(bytes)
}

/// Fetch a raw envelope from S3: returns (header, raw_stored_payload) without
/// decoding the payload. Used for delta chain resolution where we need to
/// inspect the codec before deciding how to handle the payload.
async fn fetch_raw_envelope(
    backend: &dyn BackendObjectStore,
    path: &Path,
    id: ContentId,
) -> Result<(codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
    let result = backend.get(path).await.map_err(|error| match error {
        object_store::Error::NotFound { .. } => ContentStoreError::Missing(id),
        error => ContentStoreError::Io(std::io::Error::other(error)),
    })?;
    let physical_length = result.meta.size;
    if physical_length < codec::ENVELOPE_HEADER_LEN as u64 {
        return Err(codec_error(
            id,
            usize::MAX,
            codec::CodecError::Format("bad envelope prefix"),
        ));
    }

    let mut stream = result.into_stream();
    let mut header_bytes = [0_u8; codec::ENVELOPE_HEADER_LEN];
    let mut header_filled = 0;
    let mut header = None;
    let mut stored = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
        let mut chunk = chunk.as_ref();
        if header_filled < codec::ENVELOPE_HEADER_LEN {
            let take = (codec::ENVELOPE_HEADER_LEN - header_filled).min(chunk.len());
            header_bytes[header_filled..header_filled + take].copy_from_slice(&chunk[..take]);
            header_filled += take;
            chunk = &chunk[take..];
            if header_filled == codec::ENVELOPE_HEADER_LEN {
                let parsed = codec::parse_envelope_header(&header_bytes)
                    .map_err(|error| codec_error(id, usize::MAX, error))?;
                // Validate physical length match but not logical limits (delta
                // codecs may have logical_length != stored_length, and the max
                // bound is checked after reconstruction).
                if parsed
                    .physical_length()
                    .map_err(|e| codec_error(id, usize::MAX, e))?
                    != physical_length
                {
                    return Err(codec_error(
                        id,
                        usize::MAX,
                        codec::CodecError::Format("stored length differs"),
                    ));
                }
                let capacity = usize::try_from(parsed.stored_length).map_err(|_| {
                    codec_error(
                        id,
                        usize::MAX,
                        codec::CodecError::Format("stored length does not fit this platform"),
                    )
                })?;
                stored = Some(Vec::with_capacity(capacity));
                header = Some(parsed);
            }
        }
        if !chunk.is_empty() {
            let payload = stored.as_mut().ok_or_else(|| {
                codec_error(
                    id,
                    usize::MAX,
                    codec::CodecError::Format("bad envelope prefix"),
                )
            })?;
            let expected = header
                .expect("payload allocation follows parsed header")
                .stored_length;
            if payload.len().saturating_add(chunk.len()) > expected as usize {
                return Err(codec_error(
                    id,
                    usize::MAX,
                    codec::CodecError::Format("stored length differs"),
                ));
            }
            payload.extend_from_slice(chunk);
        }
    }
    let header = header.ok_or_else(|| {
        codec_error(
            id,
            usize::MAX,
            codec::CodecError::Format("bad envelope prefix"),
        )
    })?;
    let payload = stored.expect("parsed header allocates payload");
    if payload.len() as u64 != header.stored_length {
        return Err(codec_error(
            id,
            usize::MAX,
            codec::CodecError::Format("stored length differs"),
        ));
    }
    Ok((header, payload))
}

/// Fetches only an envelope's fixed header with a ranged read, checking the object's
/// full size against it. The payload is not downloaded.
async fn fetch_envelope_header(
    backend: &dyn BackendObjectStore,
    path: &Path,
    id: ContentId,
) -> Result<codec::EnvelopeHeader, ContentStoreError> {
    let header_length = codec::ENVELOPE_HEADER_LEN as u64;
    let result = backend
        .get_opts(
            path,
            object_store::GetOptions::new().with_range(Some(0..header_length)),
        )
        .await
        .map_err(|error| match error {
            object_store::Error::NotFound { .. } => ContentStoreError::Missing(id),
            error => ContentStoreError::Io(std::io::Error::other(error)),
        })?;
    let physical_length = result.meta.size;
    let bytes = result
        .bytes()
        .await
        .map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
    if bytes.len() != codec::ENVELOPE_HEADER_LEN {
        return Err(codec_error(
            id,
            usize::MAX,
            codec::CodecError::Format("bad envelope prefix"),
        ));
    }
    let header =
        codec::parse_envelope_header(&bytes).map_err(|error| codec_error(id, usize::MAX, error))?;
    if header
        .physical_length()
        .map_err(|error| codec_error(id, usize::MAX, error))?
        != physical_length
    {
        return Err(codec_error(
            id,
            usize::MAX,
            codec::CodecError::Format("stored length differs"),
        ));
    }
    Ok(header)
}

/// S3-backed content storage with one key per ContentId.
///
/// Layout: `{prefix}/content/{first2hex}/{resthex}`
pub(super) struct RemoteContentBackend {
    backend: Arc<dyn BackendObjectStore>,
    prefix: String,
}

impl std::fmt::Debug for RemoteContentBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContentBackend")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RemoteContentBackend {
    pub(super) fn new(backend: Arc<dyn BackendObjectStore>, prefix: String) -> Self {
        Self { backend, prefix }
    }

    fn path(&self, id: ContentId) -> Path {
        let hex = hex::encode(id.as_bytes());
        Path::from(format!(
            "{}/content/{}/{}",
            self.prefix,
            &hex[..2],
            &hex[2..]
        ))
    }
}

impl ContentBackend for RemoteContentBackend {
    fn put_many(&self, contents: &[VerifiedContent<&[u8]>]) -> Result<(), ContentStoreError> {
        // ContentBackend is synchronous, but the S3 backend is async.
        // Use a blocking runtime handle to bridge.
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;

        // Each PUT is independent, but bound both request fan-out and the number
        // of encoded envelopes retained while S3 is slower than compression.
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                stream::iter(contents.iter().map(|content| async move {
                    let id = content.id();
                    let encoded = codec::encode_envelope(content.bytes())
                        .map_err(|error| codec_error(id, usize::MAX, error))?;
                    self.backend
                        .put(&self.path(id), object_store::PutPayload::from(encoded))
                        .await
                        .map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
                    Ok::<(), ContentStoreError>(())
                }))
                .buffer_unordered(MAX_CONCURRENT_REQUESTS)
                .try_collect::<Vec<_>>()
                .await
                .map(|_| ())
            })
        })
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        crate::content_store::resolve_delta_chain(id, max, |current_id| {
            self.get_raw_envelope(current_id)
        })
    }

    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
        let path = self.path(id);
        tokio::task::block_in_place(|| {
            rt.block_on(fetch_raw_envelope(self.backend.as_ref(), &path, id))
        })
    }

    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
        let path = self.path(id);
        tokio::task::block_in_place(|| {
            rt.block_on(fetch_envelope_header(self.backend.as_ref(), &path, id))
        })
        .map(|header| header.logical_length)
    }

    fn get_many_bounded(
        &self,
        ids: &[ContentId],
        max: usize,
    ) -> Option<Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError>> {
        // Use resolve_delta_chain for each item — this handles delta resolution
        // transparently. For non-delta content, it's equivalent to the old path.
        Some(
            ids.iter()
                .copied()
                .map(|id| self.get_bounded(id, max).map(|bytes| (id, bytes)))
                .collect(),
        )
    }

    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(rt) => rt,
            Err(e) => return Some(Err(ContentStoreError::Io(std::io::Error::other(e)))),
        };
        let prefix = Path::from(format!("{}/content", self.prefix));
        Some(tokio::task::block_in_place(|| {
            rt.block_on(async {
                let mut ids = Vec::new();
                let mut stream = self.backend.list(Some(&prefix));
                while let Some(meta) = stream
                    .try_next()
                    .await
                    .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?
                {
                    // Path is {prefix}/content/{first2hex}/{resthex}
                    // Extract the two parts and reconstruct the full hex digest.
                    let path_str = meta.location.to_string();
                    let content_suffix =
                        path_str.strip_prefix(&format!("{}/content/", self.prefix));
                    if let Some(suffix) = content_suffix {
                        // suffix is "{first2hex}/{resthex}"
                        let hex: String = suffix.chars().filter(|c| *c != '/').collect();
                        if hex.len() == 64
                            && let Ok(bytes) = hex::decode(&hex)
                            && let Ok(digest) = <[u8; 32]>::try_from(bytes.as_slice())
                        {
                            ids.push(ContentId::from_digest(digest));
                        }
                    }
                }
                Ok(ids)
            })
        }))
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                stream::iter(envelopes.iter().map(|(id, envelope_bytes)| async move {
                    self.backend
                        .put(
                            &self.path(*id),
                            object_store::PutPayload::from(envelope_bytes.clone()),
                        )
                        .await
                        .map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
                    Ok::<(), ContentStoreError>(())
                }))
                .buffer_unordered(MAX_CONCURRENT_REQUESTS)
                .try_collect::<Vec<_>>()
                .await
                .map(|_| ())
            })
        })
    }

    fn supports_raw_envelopes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use object_store::{PutPayload, memory::InMemory};

    use super::*;

    #[tokio::test]
    async fn range_fetch_validates_header_bounds_and_exact_physical_size() {
        let backend = InMemory::new();
        let path = Path::from("content");
        let logical = vec![b'x'; 4096];
        let id = ContentId::of(&logical);
        let encoded = codec::encode_envelope(&logical).unwrap();
        backend
            .put(&path, PutPayload::from(encoded.clone()))
            .await
            .unwrap();
        assert!(matches!(
            fetch_bounded_envelope(&backend, &path, id, logical.len() - 1).await,
            Err(ContentStoreError::TooLarge { .. })
        ));
        assert_eq!(
            fetch_bounded_envelope(&backend, &path, id, logical.len())
                .await
                .unwrap(),
            logical
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        backend
            .put(&path, PutPayload::from(trailing))
            .await
            .unwrap();
        assert!(matches!(
            fetch_bounded_envelope(&backend, &path, id, usize::MAX).await,
            Err(ContentStoreError::InvalidFormat { .. })
        ));

        backend
            .put(
                &path,
                PutPayload::from(encoded[..encoded.len() - 1].to_vec()),
            )
            .await
            .unwrap();
        assert!(matches!(
            fetch_bounded_envelope(&backend, &path, id, usize::MAX).await,
            Err(ContentStoreError::InvalidFormat { .. })
        ));
    }

    #[tokio::test]
    async fn header_fetch_reads_the_length_and_checks_the_object_size() {
        let backend = InMemory::new();
        let path = Path::from("content");
        let logical = vec![b'y'; 4096];
        let id = ContentId::of(&logical);
        let missing = fetch_envelope_header(&backend, &path, id).await;
        assert!(matches!(missing, Err(ContentStoreError::Missing(actual)) if actual == id));

        let encoded = codec::encode_envelope(&logical).unwrap();
        backend
            .put(&path, PutPayload::from(encoded.clone()))
            .await
            .unwrap();
        let header = fetch_envelope_header(&backend, &path, id).await.unwrap();
        assert_eq!(header.logical_length, logical.len() as u64);

        let mut trailing = encoded.clone();
        trailing.push(0);
        backend
            .put(&path, PutPayload::from(trailing))
            .await
            .unwrap();
        assert!(matches!(
            fetch_envelope_header(&backend, &path, id).await,
            Err(ContentStoreError::InvalidFormat { .. })
        ));

        backend
            .put(&path, PutPayload::from(encoded[..10].to_vec()))
            .await
            .unwrap();
        assert!(matches!(
            fetch_envelope_header(&backend, &path, id).await,
            Err(ContentStoreError::InvalidFormat { .. })
        ));
    }

    #[tokio::test]
    async fn range_fetch_rejects_forged_huge_logical_length_from_header() {
        let backend = InMemory::new();
        let path = Path::from("content");
        let id = ContentId::of(b"x");
        let mut encoded = codec::encode_envelope(b"x").unwrap();
        let header_length = codec::ENVELOPE_HEADER_LEN;
        let mut header = codec::parse_envelope_header(&encoded[..header_length]).unwrap();
        header.logical_length = u64::MAX;
        encoded[..header_length].copy_from_slice(&codec::encode_envelope_header(header).unwrap());
        backend.put(&path, PutPayload::from(encoded)).await.unwrap();
        assert!(matches!(
            fetch_bounded_envelope(&backend, &path, id, usize::MAX).await,
            Err(ContentStoreError::TooLarge { .. })
        ));
    }
}
