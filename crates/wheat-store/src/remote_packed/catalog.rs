//! Remote catalog chain — in-memory index rebuilt from S3-stored catalog records.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

use object_store::{ObjectStore as BackendObjectStore, ObjectStoreExt, path::Path};
use wheat_core::{
    SegmentCatalogRecord, SegmentId, SegmentIndexEntry, decode_segment_catalog_record,
    encode_segment_catalog_record,
};

use crate::packed::segment::{SegmentError, SegmentReader};

/// Entry in the in-memory locator index. Physical codec and both lengths are
/// retained so future reads can request exactly the stored range and allocate
/// only after enforcing the logical bound.
#[derive(Clone, Debug)]
#[allow(dead_code)] // metadata reserved for future range-read optimization
pub(super) struct LocatorEntry {
    pub segment_id: SegmentId,
    pub kind: u16,
    pub offset: u64,
    pub codec: u8,
    pub stored_length: u64,
    pub logical_length: u64,
}

/// In-memory catalog state, reconstructed from the S3 catalog chain.
pub(super) struct RemoteCatalog {
    /// Maps (namespace, digest) → locator entry for all known objects.
    pub index: HashMap<(u8, [u8; 32]), LocatorEntry>,
    /// All segment IDs in chain order (oldest first).
    pub segments: Vec<SegmentId>,
    /// Current head generation (0 if no segments yet).
    pub head_generation: u64,
    /// Blake3 digest of the current head catalog record (None if empty).
    pub head_digest: Option<[u8; 32]>,
    /// Segment bytes downloaded during `load()`, available for pre-warming
    /// the segment cache so the first `get()` doesn't re-download them.
    pub preloaded_segments: Vec<(SegmentId, Vec<u8>)>,
}

/// The JSON format for the catalog head pointer stored at `{prefix}/catalog/head`.
#[derive(serde::Serialize, serde::Deserialize)]
struct HeadPointer {
    generation: u64,
    digest: String,
}

impl RemoteCatalog {
    /// Create an empty catalog (for fresh repositories).
    pub(super) fn empty() -> Self {
        Self {
            index: HashMap::new(),
            segments: Vec::new(),
            head_generation: 0,
            head_digest: None,
            preloaded_segments: Vec::new(),
        }
    }

    /// Reconstruct the catalog from the S3 chain.
    ///
    /// Downloads the head pointer, walks the catalog chain to genesis,
    /// then downloads and authenticates each segment to extract its index.
    pub(super) async fn load(
        backend: &Arc<dyn BackendObjectStore>,
        prefix: &str,
    ) -> Result<Self, CatalogError> {
        // Read the head pointer.
        let head_path = Path::from(format!("{prefix}/catalog/head"));
        let head_bytes = match backend.get(&head_path).await {
            Ok(result) => result.bytes().await.map_err(CatalogError::Backend)?,
            Err(object_store::Error::NotFound { .. }) => return Ok(Self::empty()),
            Err(e) => return Err(CatalogError::Backend(e)),
        };
        let head: HeadPointer =
            serde_json::from_slice(&head_bytes).map_err(CatalogError::HeadFormat)?;
        let head_digest = hex_to_digest(&head.digest)?;

        // Walk the catalog chain from head to genesis, collecting records.
        let mut chain = Vec::new();
        let mut current_digest = Some(head_digest);
        let mut current_generation = head.generation;
        while let Some(digest) = current_digest {
            let gen_path = Path::from(format!(
                "{prefix}/catalog/{current_generation}-{}.catalog",
                hex::encode(digest)
            ));
            let catalog_bytes = match backend.get(&gen_path).await {
                Ok(result) => result.bytes().await.map_err(CatalogError::Backend)?,
                Err(object_store::Error::NotFound { .. }) => {
                    // The generation in the filename might not match chain.len().
                    // Try to find the record by listing catalog/ entries with the digest suffix.
                    find_catalog_record(backend, prefix, digest).await?
                }
                Err(e) => return Err(CatalogError::Backend(e)),
            };
            // Verify the catalog record digest.
            let actual_digest = *blake3::hash(&catalog_bytes).as_bytes();
            if actual_digest != digest {
                return Err(CatalogError::Corrupt(
                    "catalog record digest does not match chain pointer".into(),
                ));
            }
            let record = decode_segment_catalog_record(&catalog_bytes)
                .map_err(|error| CatalogError::Corrupt(format!("decode catalog: {error}")))?;
            current_digest = record.previous;
            current_generation = current_generation
                .checked_sub(1)
                .ok_or_else(|| CatalogError::Corrupt("catalog generation underflow".into()))?;
            chain.push(record);
        }

        // Reverse so chain is oldest-first.
        chain.reverse();

        // Download each segment's index and keep the raw bytes for cache pre-warming.
        let mut index = HashMap::new();
        let mut segments = Vec::new();
        let mut preloaded_segments = Vec::new();
        for record in &chain {
            let segment_id = SegmentId(record.segment);
            segments.push(segment_id);

            let seg_path = Path::from(format!(
                "{prefix}/segments/{}.segment",
                hex::encode(record.segment)
            ));
            let seg_bytes = backend
                .get(&seg_path)
                .await
                .map_err(CatalogError::Backend)?
                .bytes()
                .await
                .map_err(CatalogError::Backend)?;

            // Write to a cursor and authenticate via SegmentReader.
            let mut tmp = tempfile::tempfile().map_err(CatalogError::Io)?;
            std::io::Write::write_all(&mut tmp, &seg_bytes).map_err(CatalogError::Io)?;
            tmp.seek(SeekFrom::Start(0)).map_err(CatalogError::Io)?;

            let reader = SegmentReader::open(tmp).map_err(CatalogError::Segment)?;
            for entry in reader.authenticated_entries() {
                index.insert(
                    (entry.namespace, entry.digest),
                    LocatorEntry {
                        segment_id,
                        kind: entry.kind,
                        offset: entry.offset,
                        codec: entry.codec,
                        stored_length: entry.stored_length,
                        logical_length: entry.logical_length,
                    },
                );
            }

            // Keep the raw bytes so we can pre-warm the segment cache.
            preloaded_segments.push((segment_id, seg_bytes.to_vec()));
        }

        Ok(Self {
            index,
            segments,
            head_generation: head.generation,
            head_digest: Some(head_digest),
            preloaded_segments,
        })
    }

    /// Record a newly sealed segment: update the in-memory index, upload the
    /// catalog record and head pointer.
    pub(super) async fn seal_segment(
        &mut self,
        backend: &Arc<dyn BackendObjectStore>,
        prefix: &str,
        segment_id: SegmentId,
        entries: &[SegmentIndexEntry],
        segment_bytes: &[u8],
    ) -> Result<(), CatalogError> {
        let new_generation = self.head_generation + 1;

        // Upload the segment.
        let seg_path = Path::from(format!(
            "{prefix}/segments/{}.segment",
            hex::encode(segment_id.0)
        ));
        backend
            .put(
                &seg_path,
                object_store::PutPayload::from(segment_bytes.to_vec()),
            )
            .await
            .map_err(CatalogError::Backend)?;

        // Build and upload the catalog record.
        let record = SegmentCatalogRecord {
            generation: new_generation,
            previous: self.head_digest,
            segment: segment_id.0,
            stored_length: segment_bytes.len() as u64,
        };
        let record_bytes = encode_segment_catalog_record(&record)
            .map_err(|error| CatalogError::Corrupt(format!("encode catalog: {error}")))?;
        let record_digest = *blake3::hash(&record_bytes).as_bytes();
        let cat_path = Path::from(format!(
            "{prefix}/catalog/{}-{}.catalog",
            new_generation,
            hex::encode(record_digest)
        ));
        backend
            .put(&cat_path, object_store::PutPayload::from(record_bytes))
            .await
            .map_err(CatalogError::Backend)?;

        // Update the head pointer.
        let head_pointer = HeadPointer {
            generation: new_generation,
            digest: hex::encode(record_digest),
        };
        let head_bytes = serde_json::to_vec(&head_pointer).map_err(CatalogError::HeadFormat)?;
        let head_path = Path::from(format!("{prefix}/catalog/head"));
        backend
            .put(&head_path, object_store::PutPayload::from(head_bytes))
            .await
            .map_err(CatalogError::Backend)?;

        // Update in-memory state.
        for entry in entries {
            self.index.insert(
                (entry.namespace, entry.digest),
                LocatorEntry {
                    segment_id,
                    kind: entry.kind,
                    offset: entry.offset,
                    codec: entry.codec,
                    stored_length: entry.stored_length,
                    logical_length: entry.logical_length,
                },
            );
        }
        self.segments.push(segment_id);
        self.head_generation = new_generation;
        self.head_digest = Some(record_digest);

        Ok(())
    }
}

/// Find a catalog record by digest when the generation-based path doesn't match.
async fn find_catalog_record(
    backend: &Arc<dyn BackendObjectStore>,
    prefix: &str,
    digest: [u8; 32],
) -> Result<bytes::Bytes, CatalogError> {
    let digest_hex = hex::encode(digest);
    let catalog_prefix = Path::from(format!("{prefix}/catalog/"));
    let mut listing = backend.list(Some(&catalog_prefix));
    use futures::StreamExt;
    while let Some(entry) = listing.next().await {
        let entry = entry.map_err(CatalogError::Backend)?;
        let name = entry.location.filename().unwrap_or_default();
        if name.ends_with(&format!("-{digest_hex}.catalog")) {
            let bytes = backend
                .get(&entry.location)
                .await
                .map_err(CatalogError::Backend)?
                .bytes()
                .await
                .map_err(CatalogError::Backend)?;
            return Ok(bytes);
        }
    }
    Err(CatalogError::Corrupt(
        "catalog record not found in chain".into(),
    ))
}

fn hex_to_digest(hex_str: &str) -> Result<[u8; 32], CatalogError> {
    let bytes = hex::decode(hex_str)
        .map_err(|_| CatalogError::Corrupt("invalid hex in head pointer".into()))?;
    bytes
        .try_into()
        .map_err(|_| CatalogError::Corrupt("head pointer digest is not 32 bytes".into()))
}

#[derive(Debug, thiserror::Error)]
pub(super) enum CatalogError {
    #[error("S3 backend error: {0}")]
    Backend(object_store::Error),
    #[error("catalog I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("segment error: {0}")]
    Segment(#[from] SegmentError),
    #[error("catalog corrupt: {0}")]
    Corrupt(String),
    #[error("head pointer format error: {0}")]
    HeadFormat(#[from] serde_json::Error),
}
