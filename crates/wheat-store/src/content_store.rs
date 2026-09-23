//! Content retention facade and loose reference backend.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use thiserror::Error;
use wheat_core::ContentId;

use crate::{Durability, codec};

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
        codec::CodecError::Format(_)
        | codec::CodecError::Unknown(_)
        | codec::CodecError::Zstd(_)
        | codec::CodecError::Delta(_) => ContentStoreError::InvalidFormat {
            id,
            reason: error.to_string(),
        },
    }
}

/// Maximum number of delta links to follow before giving up.
pub(crate) const MAX_DELTA_CHAIN_DEPTH: usize = 16;

#[derive(Debug, Error)]
pub enum ContentStoreError {
    #[error("content {0} is missing")]
    Missing(ContentId),
    #[error("content {expected} failed integrity verification (found {actual})")]
    Corrupt {
        expected: ContentId,
        actual: ContentId,
    },
    #[error("content {id} changed while it was being read")]
    Changed { id: ContentId },
    #[error("content {id} has an invalid physical envelope: {reason}")]
    InvalidFormat { id: ContentId, reason: String },
    #[error("content {id} is at least {minimum} bytes, exceeding the read limit of {max} bytes")]
    TooLarge {
        id: ContentId,
        max: usize,
        /// A trustworthy lower bound derived before rejecting the read.
        minimum: usize,
    },
    #[error("content store I/O failed")]
    Io(#[from] io::Error),
    #[error("content {id} exceeds delta chain depth limit of {MAX_DELTA_CHAIN_DEPTH}")]
    DeltaChainTooDeep { id: ContentId },
    #[error("content {id} delta resolution failed: {reason}")]
    DeltaResolution { id: ContentId, reason: String },
}

pub(crate) trait ContentBackend: std::fmt::Debug + Send + Sync {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError>;
    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError>;

    /// Read the raw envelope for a content ID: header + stored payload bytes.
    ///
    /// Used by `resolve_delta_chain` for iterative delta resolution.
    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(crate::codec::EnvelopeHeader, Vec<u8>), ContentStoreError>;

    /// Write pre-encoded envelope bytes directly to storage.
    ///
    /// Used for storing delta-encoded content where the envelope has already
    /// been constructed by the caller. Each pair is `(ContentId, envelope_bytes)`.
    fn put_raw_envelopes(
        &self,
        _envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        Err(ContentStoreError::Io(io::Error::other(
            "put_raw_envelopes not supported by this backend",
        )))
    }

    /// Whether this backend supports `put_raw_envelopes`.
    ///
    /// Backends that store content in their own packed format (e.g. segment
    /// packs) cannot accept pre-encoded envelopes. Callers use this to skip
    /// delta computation when it would be wasted work.
    fn supports_raw_envelopes(&self) -> bool {
        false
    }

    /// List all content IDs stored in this backend.
    ///
    /// Returns `None` if the backend does not support listing (the caller
    /// should fall back to per-item `get_bounded` existence checks).
    /// Remote backends override this to use a single LIST/scan call
    /// instead of one GET per item.
    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        None
    }

    /// Fetch multiple content blobs concurrently.
    ///
    /// Returns `None` if the backend does not support concurrent reads
    /// (the caller should fall back to sequential `get_bounded` calls).
    /// Remote backends override this to issue concurrent S3 GETs.
    #[allow(clippy::type_complexity)]
    fn get_many_bounded(
        &self,
        _ids: &[ContentId],
        _max: usize,
    ) -> Option<Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError>> {
        None
    }
}

/// Iteratively resolve a content delta chain and verify the final ContentId.
///
/// `fetch_raw` is a closure that reads the raw envelope for a given ContentId,
/// returning `(EnvelopeHeader, raw_stored_payload)`. The delta chain is followed
/// until a non-delta codec is found, then patches are applied in reverse
/// (base → target) to reconstruct the requested content.
///
/// The `max` bound applies only to the final reconstructed output.
pub(crate) fn resolve_delta_chain(
    id: ContentId,
    max: usize,
    fetch_raw: impl Fn(ContentId) -> Result<(codec::EnvelopeHeader, Vec<u8>), ContentStoreError>,
) -> Result<Vec<u8>, ContentStoreError> {
    use crate::delta;

    let mut chain: Vec<Vec<u8>> = Vec::new();
    let mut current_id = id;

    for depth in 0..=MAX_DELTA_CHAIN_DEPTH {
        let (header, stored) = fetch_raw(current_id)?;

        match codec::decode_payload(header.codec, stored, header.logical_length, usize::MAX)
            .map_err(|e| codec_error(current_id, max, e))?
        {
            codec::DecodedPayload::Content(bytes) => {
                // Non-delta codec — this is the base of the chain.
                if chain.is_empty() {
                    // No delta chain at all — direct content.
                    if bytes.len() > max {
                        return Err(ContentStoreError::TooLarge {
                            id,
                            max,
                            minimum: bytes.len(),
                        });
                    }
                    let actual = ContentId::of(&bytes);
                    if actual != id {
                        return Err(ContentStoreError::Corrupt {
                            expected: id,
                            actual,
                        });
                    }
                    return Ok(bytes);
                }
                // Apply patches in reverse order (base → target).
                let mut result = bytes;
                for patch in chain.iter().rev() {
                    result = delta::apply_delta(&result, patch).map_err(|e| {
                        ContentStoreError::DeltaResolution {
                            id,
                            reason: e.to_string(),
                        }
                    })?;
                }
                if result.len() > max {
                    return Err(ContentStoreError::TooLarge {
                        id,
                        max,
                        minimum: result.len(),
                    });
                }
                let actual = ContentId::of(&result);
                if actual != id {
                    return Err(ContentStoreError::Corrupt {
                        expected: id,
                        actual,
                    });
                }
                return Ok(result);
            }
            codec::DecodedPayload::Delta { base_id, patch, .. } => {
                if depth == MAX_DELTA_CHAIN_DEPTH {
                    return Err(ContentStoreError::DeltaChainTooDeep { id });
                }
                chain.push(patch);
                current_id = base_id;
            }
        }
    }

    unreachable!("bounded delta-chain loop always returns")
}

#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
    backend: Option<Arc<dyn ContentBackend>>,
    durability: Durability,
}

impl ContentStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_durability(root, Durability::Durable)
    }

    #[must_use]
    pub fn with_durability(root: impl Into<PathBuf>, durability: Durability) -> Self {
        Self {
            root: root.into(),
            backend: None,
            durability,
        }
    }

    pub(crate) fn with_backend(
        root: PathBuf,
        backend: Arc<dyn ContentBackend>,
        durability: Durability,
    ) -> Self {
        Self {
            root,
            backend: Some(backend),
            durability,
        }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ContentId, ContentStoreError> {
        let id = ContentId::of(bytes);
        self.put_with_id(id, bytes)?;
        Ok(id)
    }

    /// Retains already-addressed content with bounded parallel durability work.
    /// Every claimed ID is verified before publication.
    pub fn put_many(
        &self,
        contents: impl IntoIterator<Item = (ContentId, Vec<u8>)>,
        concurrency: usize,
    ) -> Result<(), ContentStoreError> {
        let mut deduplicated = std::collections::BTreeMap::new();
        for (id, bytes) in contents {
            let actual = ContentId::of(&bytes);
            if actual != id {
                return Err(ContentStoreError::Corrupt {
                    expected: id,
                    actual,
                });
            }
            if let Some(previous) = deduplicated.get(&id) {
                if previous != &bytes {
                    return Err(ContentStoreError::Corrupt {
                        expected: id,
                        actual,
                    });
                }
            } else {
                deduplicated.insert(id, bytes);
            }
        }
        if deduplicated.is_empty() {
            return Ok(());
        }
        if let Some(backend) = &self.backend {
            return backend.put_many(&deduplicated.into_iter().collect::<Vec<_>>());
        }
        let workers = concurrency.max(1).min(deduplicated.len());
        let (sender, receiver) = mpsc::channel();
        for content in deduplicated {
            sender.send(content).expect("receiver remains alive");
        }
        drop(sender);
        let receiver = Arc::new(Mutex::new(receiver));
        thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    let receiver = Arc::clone(&receiver);
                    scope.spawn(move || -> Result<(), ContentStoreError> {
                        loop {
                            let next = receiver.lock().expect("worker mutex poisoned").recv();
                            let Ok((id, bytes)) = next else {
                                return Ok(());
                            };
                            self.put_with_id(id, &bytes)?;
                        }
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().expect("content worker panicked")?;
            }
            Ok(())
        })
    }

    /// Store content with an optional delta base hint.
    ///
    /// If `base_id` refers to content already in this store, and a delta
    /// against it is smaller than the raw content, stores the delta envelope
    /// instead. Otherwise falls back to raw storage. The identity of `bytes`
    /// must match `id`; this is always verified.
    pub fn put_with_base(
        &self,
        id: ContentId,
        bytes: &[u8],
        base_id: ContentId,
    ) -> Result<(), ContentStoreError> {
        if ContentId::of(bytes) != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual: ContentId::of(bytes),
            });
        }
        // Skip delta if the content already exists.
        if self.contains(id) {
            return Ok(());
        }
        // Backends that don't support raw envelope writes (e.g. packed stores)
        // cannot store delta-encoded content — skip the delta attempt entirely.
        if self
            .backend
            .as_ref()
            .is_some_and(|b| !b.supports_raw_envelopes())
        {
            return self.put_with_id(id, bytes);
        }
        // Keep the stored chain within the same 16-link bound enforced by
        // resolution. A chain with 16 deltas still needs one final full-base
        // read, so the resolver deliberately permits that terminal fetch.
        let base_depth = self
            .delta_chain_depth(base_id)
            .unwrap_or(MAX_DELTA_CHAIN_DEPTH);
        if base_depth >= MAX_DELTA_CHAIN_DEPTH {
            return self.put_with_id(id, bytes);
        }
        // Try to compute a delta against the base content.
        let base_bytes = match self.get(base_id) {
            Ok(base) => base,
            Err(ContentStoreError::Missing(_)) => {
                return self.put_with_id(id, bytes);
            }
            Err(e) => return Err(e),
        };
        if let Some(patch) = crate::delta::compute_delta(&base_bytes, bytes) {
            let envelope = codec::encode_delta_envelope(bytes.len(), base_id, &patch)
                .map_err(|e| codec_error(id, usize::MAX, e))?;
            self.put_raw_envelope(id, envelope)
        } else {
            self.put_with_id(id, bytes)
        }
    }

    /// Store content from a batch, with optional delta base hints.
    ///
    /// Each item is `(content_id, content_bytes, optional_base_id)`. Items
    /// with a base hint attempt delta encoding; items without store raw.
    pub fn put_many_with_bases(
        &self,
        contents: impl IntoIterator<Item = (ContentId, Vec<u8>, Option<ContentId>)>,
        concurrency: usize,
    ) -> Result<(), ContentStoreError> {
        // Split into delta-eligible and raw items.
        let mut raw_items = Vec::new();
        for (id, bytes, base_id) in contents {
            let actual = ContentId::of(&bytes);
            if actual != id {
                return Err(ContentStoreError::Corrupt {
                    expected: id,
                    actual,
                });
            }
            if let Some(base) = base_id {
                // Try delta — fall back to raw on failure.
                if let Err(_e) = self.put_with_base(id, &bytes, base) {
                    raw_items.push((id, bytes));
                }
            } else {
                raw_items.push((id, bytes));
            }
        }
        if !raw_items.is_empty() {
            self.put_many(raw_items, concurrency)?;
        }
        Ok(())
    }

    /// Write a pre-encoded envelope directly to the store.
    pub(crate) fn put_raw_envelope(
        &self,
        id: ContentId,
        envelope: Vec<u8>,
    ) -> Result<(), ContentStoreError> {
        if let Some(backend) = &self.backend {
            return backend.put_raw_envelopes(&[(id, envelope)]);
        }
        // Local filesystem: write the envelope bytes directly.
        let path = self.path(id);
        let parent = path.parent().expect("content path has parent");
        fs::create_dir_all(parent)?;

        let mut directory = Some(nonempty_path(&self.root));
        while let Some(path) = directory {
            self.durability.sync_directory(path)?;
            directory = nonempty_parent(path);
        }

        if path.exists() {
            // Already stored (possibly by a concurrent writer).
            return Ok(());
        }

        let temporary = parent.join(format!(".{}.{}.tmp", id, std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&envelope)?;
            self.durability.sync_file(&file)?;
            match fs::rename(&temporary, &path) {
                Ok(()) => {
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) => Err(ContentStoreError::Io(error)),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn put_with_id(&self, id: ContentId, bytes: &[u8]) -> Result<(), ContentStoreError> {
        if ContentId::of(bytes) != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual: ContentId::of(bytes),
            });
        }
        if let Some(backend) = &self.backend {
            return backend.put_many(&[(id, bytes.to_vec())]);
        }
        let path = self.path(id);
        let parent = path.parent().expect("content path has parent");
        fs::create_dir_all(parent)?;

        // Persist both directory entries needed to reach a content file. Do this
        // even when the directories already exist: they may be leftovers from a
        // previous attempt whose directory sync failed, and must not become an
        // implicitly acknowledged success on retry.
        let mut directory = Some(nonempty_path(&self.root));
        while let Some(path) = directory {
            self.durability.sync_directory(path)?;
            directory = nonempty_parent(path);
        }

        if path.exists() {
            self.verify_existing(id, bytes)?;
            // The file may have been renamed by an attempt that subsequently
            // failed its parent-directory sync.
            self.durability.sync_directory(parent)?;
            return Ok(());
        }

        let encoded =
            codec::encode_envelope(bytes).map_err(|error| codec_error(id, usize::MAX, error))?;
        let temporary = parent.join(format!(".{}.{}.tmp", id, std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&encoded)?;
            self.durability.sync_file(&file)?;
            match fs::rename(&temporary, &path) {
                Ok(()) => {
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.verify_existing(id, bytes)?;
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) => Err(ContentStoreError::Io(error)),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn get(&self, id: ContentId) -> Result<Vec<u8>, ContentStoreError> {
        self.get_bounded(id, usize::MAX)
    }

    /// Reads and verifies content while refusing to allocate more than `max` bytes for it.
    ///
    /// Transparently resolves delta chains: if the envelope uses a delta codec,
    /// follows the base reference chain (up to [`MAX_DELTA_CHAIN_DEPTH`] links)
    /// and reconstructs the content by applying patches in reverse. The `max`
    /// bound applies to the final reconstructed output, not to intermediate
    /// base fetches.
    pub fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        if let Some(backend) = &self.backend {
            return resolve_delta_chain(id, max, |current_id| backend.get_raw_envelope(current_id));
        }
        resolve_delta_chain(id, max, |current_id| {
            self.read_local_raw_envelope(current_id)
        })
    }

    /// Read a local envelope file and return (header, raw stored payload).
    pub(crate) fn read_local_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
        let path = self.path(id);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(id));
            }
            Err(error) => return Err(ContentStoreError::Io(error)),
        };
        let physical_length = file.metadata()?.len();
        codec::read_envelope_file_raw(&mut file, physical_length).map_err(|error| match error {
            codec::EnvelopeFileError::Codec(error) => codec_error(id, usize::MAX, error),
            codec::EnvelopeFileError::Changed => ContentStoreError::Changed { id },
            codec::EnvelopeFileError::Io(error) => ContentStoreError::Io(error),
        })
    }

    #[must_use]
    pub fn contains(&self, id: ContentId) -> bool {
        if let Some(backend) = &self.backend {
            return backend.get_bounded(id, usize::MAX).is_ok();
        }
        self.path(id).is_file()
    }

    /// Fetch multiple content blobs, using concurrent reads when the
    /// backend supports it (S3). Falls back to sequential `get_bounded`
    /// for local filesystem backends.
    pub fn get_many_bounded(
        &self,
        ids: &[ContentId],
        max: usize,
    ) -> Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError> {
        if let Some(backend) = &self.backend
            && let Some(result) = backend.get_many_bounded(ids, max)
        {
            return result;
        }
        // Fallback: sequential reads (fast for local filesystem).
        let mut results = Vec::with_capacity(ids.len());
        for &id in ids {
            let bytes = self.get_bounded(id, max)?;
            results.push((id, bytes));
        }
        Ok(results)
    }

    /// Return the set of content IDs that already exist in this store,
    /// filtered to only those in `candidates`.
    ///
    /// When the backend supports listing (remote/S3), this does one
    /// LIST call and filters in memory — O(1) network round trips
    /// instead of O(n) per-item existence checks. Falls back to
    /// per-item `contains()` for local filesystem backends.
    pub fn existing_ids(
        &self,
        candidates: &[ContentId],
    ) -> Result<std::collections::HashSet<ContentId>, ContentStoreError> {
        if let Some(backend) = &self.backend
            && let Some(result) = backend.list_ids()
        {
            let all: std::collections::HashSet<ContentId> = result?.into_iter().collect();
            return Ok(candidates
                .iter()
                .copied()
                .filter(|id| all.contains(id))
                .collect());
        }
        // Fallback: per-item check (fast for local filesystem).
        Ok(candidates
            .iter()
            .copied()
            .filter(|id| self.contains(*id))
            .collect())
    }

    /// Loose fallback path, not a locator for packed content.
    #[must_use]
    pub fn path(&self, id: ContentId) -> PathBuf {
        let encoded = id.to_string();
        self.root.join(&encoded[..2]).join(&encoded[2..])
    }

    /// Count the delta chain depth starting from `id`.
    ///
    /// Returns 0 if content is stored raw, 1 if it's a delta against raw
    /// content, 2 if it's a delta against a delta against raw, etc.
    /// Returns `None` if the content can't be inspected.
    fn delta_chain_depth(&self, id: ContentId) -> Option<usize> {
        let fetch_raw = |cid: ContentId| -> Option<(codec::EnvelopeHeader, Vec<u8>)> {
            if let Some(backend) = &self.backend {
                backend.get_raw_envelope(cid).ok()
            } else {
                self.read_local_raw_envelope(cid).ok()
            }
        };

        let mut current = id;
        for depth in 0..MAX_DELTA_CHAIN_DEPTH {
            let (header, stored) = fetch_raw(current)?;
            if !header.codec.is_delta() {
                return Some(depth);
            }
            match codec::decode_payload(header.codec, stored, header.logical_length, usize::MAX) {
                Ok(codec::DecodedPayload::Delta { base_id, .. }) => current = base_id,
                _ => return Some(depth),
            }
        }
        Some(MAX_DELTA_CHAIN_DEPTH)
    }

    fn verify_existing(
        &self,
        id: ContentId,
        expected_bytes: &[u8],
    ) -> Result<(), ContentStoreError> {
        let actual_bytes = self.get(id)?;
        if actual_bytes != expected_bytes {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual: ContentId::of(&actual_bytes),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
fn read_exact_content(
    reader: &mut impl io::Read,
    id: ContentId,
    size: usize,
) -> Result<Vec<u8>, ContentStoreError> {
    let mut bytes = vec![0; size];
    if let Err(error) = reader.read_exact(&mut bytes) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Err(ContentStoreError::Changed { id })
        } else {
            Err(ContentStoreError::Io(error))
        };
    }
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(ContentStoreError::Changed { id });
    }
    Ok(bytes)
}

fn nonempty_parent(path: &Path) -> Option<&Path> {
    if path == Path::new(".") {
        None
    } else {
        path.parent().map(nonempty_path)
    }
}

fn nonempty_path(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_content_reader_detects_shrink_and_growth() {
        let id = ContentId::of(b"expected");
        assert!(matches!(
            read_exact_content(&mut &b"short"[..], id, 8),
            Err(ContentStoreError::Changed { id: actual }) if actual == id
        ));
        assert!(matches!(
            read_exact_content(&mut &b"too long"[..], id, 3),
            Err(ContentStoreError::Changed { id: actual }) if actual == id
        ));
        assert_eq!(
            read_exact_content(&mut &b"exact"[..], id, 5).unwrap(),
            b"exact"
        );
    }
}
