//! Content retention facade and loose reference backend.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use thiserror::Error;
use wheat_core::{ContentId, ContentMismatch, VerifiedContent};

use crate::{Durability, codec};

/// Content to store, with the content it is likely a small change of.
#[derive(Clone, Copy, Debug)]
pub struct ContentItem<'a> {
    pub content: VerifiedContent<&'a [u8]>,
    /// Content to try storing `content` as a delta against.
    pub base: Option<ContentId>,
}

impl<'a> ContentItem<'a> {
    #[must_use]
    pub fn new(content: VerifiedContent<&'a [u8]>, base: Option<ContentId>) -> Self {
        Self { content, base }
    }

    #[must_use]
    pub fn id(&self) -> ContentId {
        self.content.id()
    }

    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.content.into_inner()
    }
}

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

fn envelope_file_error(id: ContentId, error: codec::EnvelopeFileError) -> ContentStoreError {
    match error {
        codec::EnvelopeFileError::Codec(error) => codec_error(id, usize::MAX, error),
        codec::EnvelopeFileError::Changed => ContentStoreError::Changed { id },
        codec::EnvelopeFileError::Io(error) => ContentStoreError::Io(error),
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

impl From<ContentMismatch> for ContentStoreError {
    fn from(mismatch: ContentMismatch) -> Self {
        Self::Corrupt {
            expected: mismatch.claimed,
            actual: mismatch.actual,
        }
    }
}

pub(crate) trait ContentBackend: std::fmt::Debug + Send + Sync {
    /// Stores `contents`, which arrive verified and are not hashed again.
    fn put_many(&self, contents: &[VerifiedContent<&[u8]>]) -> Result<(), ContentStoreError>;
    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError>;

    /// Read the raw envelope for a content ID: header + stored payload bytes.
    ///
    /// Used by `resolve_delta_chain` for iterative delta resolution.
    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(crate::codec::EnvelopeHeader, Vec<u8>), ContentStoreError>;

    /// The content's logical length from stored metadata (see
    /// [`ContentStore::logical_length`]). The default reads the raw envelope without
    /// decoding it; backends that index lengths answer without reading the payload.
    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        self.get_raw_envelope(id)
            .map(|(header, _)| header.logical_length)
    }

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

    // Each link carries the ID and envelope-declared target length that must be
    // authenticated after applying its patch. Ancestor bases may legitimately
    // exceed the caller's final-output bound, but no record may exceed the hard
    // logical-record limit.
    let mut chain: Vec<(ContentId, usize, Vec<u8>)> = Vec::new();
    let mut current_id = id;

    for depth in 0..=MAX_DELTA_CHAIN_DEPTH {
        let (header, stored) = fetch_raw(current_id)?;
        let decode_max = if depth == 0 {
            max.min(codec::HARD_LOGICAL_RECORD_BYTES)
        } else {
            codec::HARD_LOGICAL_RECORD_BYTES
        };
        let logical_length =
            usize::try_from(header.logical_length).map_err(|_| ContentStoreError::TooLarge {
                id: current_id,
                max: decode_max,
                minimum: codec::HARD_LOGICAL_RECORD_BYTES.saturating_add(1),
            })?;
        if logical_length > decode_max {
            return Err(ContentStoreError::TooLarge {
                id: current_id,
                max: decode_max,
                minimum: logical_length,
            });
        }

        match codec::decode_payload(header.codec, stored, header.logical_length, decode_max)
            .map_err(|e| codec_error(current_id, decode_max, e))?
        {
            codec::DecodedPayload::Content(bytes) => {
                // The terminal base is content-addressed too. Verify it before
                // trusting it as input to any patch in the chain.
                let actual = ContentId::of(&bytes);
                if actual != current_id {
                    return Err(ContentStoreError::Corrupt {
                        expected: current_id,
                        actual,
                    });
                }

                let mut result = bytes;
                for (link_index, (target_id, expected_size, patch)) in
                    chain.iter().rev().enumerate()
                {
                    let result_limit = if link_index + 1 == chain.len() {
                        max.min(codec::HARD_LOGICAL_RECORD_BYTES)
                    } else {
                        codec::HARD_LOGICAL_RECORD_BYTES
                    };
                    result = delta::apply_delta(&result, patch, *expected_size, result_limit)
                        .map_err(|e| ContentStoreError::DeltaResolution {
                            id: *target_id,
                            reason: e.to_string(),
                        })?;
                    let actual = ContentId::of(&result);
                    if actual != *target_id {
                        return Err(ContentStoreError::Corrupt {
                            expected: *target_id,
                            actual,
                        });
                    }
                }
                return Ok(result);
            }
            codec::DecodedPayload::Delta { base_id, patch, .. } => {
                if depth == MAX_DELTA_CHAIN_DEPTH {
                    return Err(ContentStoreError::DeltaChainTooDeep { id });
                }
                chain.push((current_id, logical_length, patch));
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

    /// This store's root and durability over `backend`, which sees this store
    /// through its own handle.
    pub(crate) fn overlaid(&self, backend: Arc<dyn ContentBackend>) -> Self {
        Self::with_backend(self.root.clone(), backend, self.durability)
    }

    /// The stored envelope of `id`: its header and stored payload.
    pub(crate) fn raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
        match &self.backend {
            Some(backend) => backend.get_raw_envelope(id),
            None => self.read_local_raw_envelope(id),
        }
    }

    /// Whether [`Self::put_raw_envelope`] can store delta envelopes here.
    pub(crate) fn supports_raw_envelopes(&self) -> bool {
        self.backend
            .as_ref()
            .is_none_or(|backend| backend.supports_raw_envelopes())
    }

    /// Every stored ID, when the backend can list them in one call.
    pub(crate) fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        self.backend.as_ref().and_then(|backend| backend.list_ids())
    }

    #[tracing::instrument(name = "content_put", level = "debug", skip_all, fields(bytes = bytes.len()))]
    pub fn put(&self, bytes: &[u8]) -> Result<ContentId, ContentStoreError> {
        let content = VerifiedContent::new(bytes);
        self.put_verified(content)?;
        Ok(content.id())
    }

    /// Retains verified content with bounded parallel durability work.
    pub fn put_many(
        &self,
        contents: &[VerifiedContent<&[u8]>],
        concurrency: usize,
    ) -> Result<(), ContentStoreError> {
        let deduplicated = contents
            .iter()
            .map(|content| (content.id(), *content))
            .collect::<std::collections::BTreeMap<_, _>>();
        if deduplicated.is_empty() {
            return Ok(());
        }
        if let Some(backend) = &self.backend {
            return backend.put_many(&deduplicated.into_values().collect::<Vec<_>>());
        }
        let workers = concurrency.max(1).min(deduplicated.len());
        let (sender, receiver) = mpsc::channel();
        for content in deduplicated.into_values() {
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
                            let Ok(content) = next else {
                                return Ok(());
                            };
                            self.put_verified(content)?;
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
    /// instead. Otherwise falls back to raw storage.
    pub fn put_with_base(
        &self,
        content: VerifiedContent<&[u8]>,
        base_id: ContentId,
    ) -> Result<(), ContentStoreError> {
        let id = content.id();
        // Skip delta if the content already exists.
        if self.contains(id) {
            return Ok(());
        }
        match self.delta_envelope(id, content.bytes(), base_id)? {
            Some(envelope) => self.put_raw_envelope(id, envelope),
            None => self.put_verified(content),
        }
    }

    /// The delta envelope storing `bytes` (whose ID is `id`) against `base_id`, or
    /// `None` when they should be stored whole: the backend takes no envelopes, the
    /// base is missing or at the end of a full chain, or no delta is smaller.
    pub(crate) fn delta_envelope(
        &self,
        id: ContentId,
        bytes: &[u8],
        base_id: ContentId,
    ) -> Result<Option<Vec<u8>>, ContentStoreError> {
        // Backends that don't support raw envelope writes (e.g. packed stores)
        // cannot store delta-encoded content — skip the delta attempt entirely.
        if self
            .backend
            .as_ref()
            .is_some_and(|b| !b.supports_raw_envelopes())
        {
            return Ok(None);
        }
        // Keep the stored chain within the same 16-link bound enforced by
        // resolution. A chain with 16 deltas still needs one final full-base
        // read, so the resolver deliberately permits that terminal fetch.
        let base_depth = self
            .delta_chain_depth(base_id)
            .unwrap_or(MAX_DELTA_CHAIN_DEPTH);
        if base_depth >= MAX_DELTA_CHAIN_DEPTH {
            return Ok(None);
        }
        // Try to compute a delta against the base content.
        let base_bytes = match self.get(base_id) {
            Ok(base) => base,
            Err(ContentStoreError::Missing(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        crate::delta::compute_delta(&base_bytes, bytes)
            .map(|patch| {
                codec::encode_delta_envelope(bytes.len(), base_id, &patch)
                    .map_err(|e| codec_error(id, usize::MAX, e))
            })
            .transpose()
    }

    /// Store content from a batch, with optional delta base hints.
    ///
    /// Items with a base hint attempt delta encoding; items without store raw.
    pub fn put_many_with_bases(
        &self,
        contents: &[ContentItem<'_>],
        concurrency: usize,
    ) -> Result<(), ContentStoreError> {
        // Split into delta-eligible and raw items.
        let mut raw_items = Vec::new();
        for item in contents {
            if let Some(base) = item.base {
                // Try delta — fall back to raw on failure.
                if let Err(_e) = self.put_with_base(item.content, base) {
                    raw_items.push(item.content);
                }
            } else {
                raw_items.push(item.content);
            }
        }
        if !raw_items.is_empty() {
            self.put_many(&raw_items, concurrency)?;
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
        self.sync_root_entries()?;

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

    /// Persists the directory entries that reach a shard directory: the shard's entry in the
    /// root and the root's entry in its parent. Do this even when the directories already
    /// exist: they may be leftovers from a previous attempt whose directory sync failed, and
    /// must not become an implicitly acknowledged success on retry. Directories above the
    /// root belong to whoever created the store, so every put costs two syncs here rather
    /// than one per ancestor up to the filesystem root.
    fn sync_root_entries(&self) -> Result<(), ContentStoreError> {
        let root = nonempty_path(&self.root);
        self.durability.sync_directory(root)?;
        if let Some(parent) = nonempty_parent(root) {
            self.durability.sync_directory(parent)?;
        }
        Ok(())
    }

    /// Stores content verified where it came in, without hashing it again.
    pub fn put_verified(&self, content: VerifiedContent<&[u8]>) -> Result<(), ContentStoreError> {
        if let Some(backend) = &self.backend {
            return backend.put_many(&[content]);
        }
        let (id, bytes) = (content.id(), content.into_inner());
        let path = self.path(id);
        let parent = path.parent().expect("content path has parent");
        fs::create_dir_all(parent)?;
        self.sync_root_entries()?;

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

    #[tracing::instrument(name = "content_get", level = "debug", skip_all)]
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
        codec::read_envelope_file_raw(&mut file, physical_length)
            .map_err(|error| envelope_file_error(id, error))
    }

    /// The byte length of `id`'s content, from stored metadata alone. No payload is read
    /// and no delta chain is resolved, so sizing work stays cheap. The length is not
    /// checked against the content itself; reading the bytes does that.
    pub fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        if let Some(backend) = &self.backend {
            return backend.logical_length(id);
        }
        self.read_local_logical_length(id)
    }

    /// The logical length recorded in a local envelope file's header.
    pub(crate) fn read_local_logical_length(
        &self,
        id: ContentId,
    ) -> Result<u64, ContentStoreError> {
        let mut file = match fs::File::open(self.path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(id));
            }
            Err(error) => return Err(ContentStoreError::Io(error)),
        };
        let physical_length = file.metadata()?.len();
        codec::read_envelope_file_header(&mut file, physical_length)
            .map(|header| header.logical_length)
            .map_err(|error| envelope_file_error(id, error))
    }

    /// Whether `id` is stored, from stored metadata alone: like
    /// [`Self::logical_length`], it reads no payload and does not check the content.
    #[must_use]
    pub fn contains(&self, id: ContentId) -> bool {
        if let Some(backend) = &self.backend {
            return backend.logical_length(id).is_ok();
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
    use std::collections::HashMap;

    use super::*;

    fn raw_envelope(bytes: &[u8]) -> (codec::EnvelopeHeader, Vec<u8>) {
        (
            codec::EnvelopeHeader {
                codec: codec::Codec::Raw,
                stored_length: bytes.len() as u64,
                logical_length: bytes.len() as u64,
            },
            bytes.to_vec(),
        )
    }

    fn delta_envelope(
        base_id: ContentId,
        target_length: usize,
        patch: Vec<u8>,
    ) -> (codec::EnvelopeHeader, Vec<u8>) {
        let mut stored = Vec::with_capacity(crate::delta::BASE_REFERENCE_SIZE + patch.len());
        stored.extend_from_slice(base_id.as_bytes());
        stored.extend_from_slice(&patch);
        (
            codec::EnvelopeHeader {
                codec: codec::Codec::Delta,
                stored_length: stored.len() as u64,
                logical_length: target_length as u64,
            },
            stored,
        )
    }

    fn resolve_from(
        envelopes: HashMap<ContentId, (codec::EnvelopeHeader, Vec<u8>)>,
        id: ContentId,
        max: usize,
    ) -> Result<Vec<u8>, ContentStoreError> {
        resolve_delta_chain(id, max, |current_id| {
            envelopes
                .get(&current_id)
                .cloned()
                .ok_or(ContentStoreError::Missing(current_id))
        })
    }

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

    #[test]
    fn delta_resolution_allows_large_base_for_small_bounded_target() {
        let base = b"large base that is larger than the requested target";
        let target = b"small";
        let base_id = ContentId::of(base);
        let target_id = ContentId::of(target);
        let patch = crate::delta::compute_delta(base, target).unwrap_or_else(|| {
            // Git delta: base length, target length, then a five-byte literal.
            vec![
                base.len() as u8,
                target.len() as u8,
                target.len() as u8,
                b's',
                b'm',
                b'a',
                b'l',
                b'l',
            ]
        });
        let envelopes = HashMap::from([
            (base_id, raw_envelope(base)),
            (target_id, delta_envelope(base_id, target.len(), patch)),
        ]);

        assert_eq!(
            resolve_from(envelopes, target_id, target.len()).unwrap(),
            target
        );
    }

    #[test]
    fn delta_resolution_enforces_request_boundary_before_patch_allocation() {
        let base = b"base";
        let target = b"base!";
        let base_id = ContentId::of(base);
        let target_id = ContentId::of(target);
        // The patch body is immaterial: the envelope target is rejected first.
        let envelopes = HashMap::from([
            (base_id, raw_envelope(base)),
            (target_id, delta_envelope(base_id, target.len(), vec![4, 5])),
        ]);

        assert!(matches!(
            resolve_from(envelopes, target_id, target.len() - 1),
            Err(ContentStoreError::TooLarge { id, max: 4, minimum: 5 }) if id == target_id
        ));
    }

    #[test]
    fn delta_resolution_rejects_patch_and_envelope_target_mismatch() {
        let base = b"base";
        let target_id = ContentId::of(b"base!");
        let base_id = ContentId::of(base);
        // Patch declares four output bytes while the envelope authenticates five.
        let envelopes = HashMap::from([
            (base_id, raw_envelope(base)),
            (target_id, delta_envelope(base_id, 5, vec![4, 4, 0x90, 4])),
        ]);

        assert!(matches!(
            resolve_from(envelopes, target_id, 5),
            Err(ContentStoreError::DeltaResolution { .. })
        ));
    }

    #[test]
    fn delta_resolution_enforces_hard_limit_even_with_unbounded_request() {
        let base_id = ContentId::of(b"base");
        let target_id = ContentId::of(b"target");
        let mut envelope = delta_envelope(base_id, 0, vec![4, 0]);
        envelope.0.logical_length = codec::HARD_LOGICAL_RECORD_BYTES as u64 + 1;
        let envelopes = HashMap::from([(target_id, envelope)]);

        assert!(matches!(
            resolve_from(envelopes, target_id, usize::MAX),
            Err(ContentStoreError::TooLarge { id, minimum, .. })
                if id == target_id && minimum == codec::HARD_LOGICAL_RECORD_BYTES + 1
        ));
    }

    #[test]
    fn delta_resolution_rejects_wrong_base_even_when_patch_replaces_every_byte() {
        let base_id = ContentId::of(b"good");
        let target = b"small";
        let target_id = ContentId::of(target);
        let patch = vec![4, 5, 5, b's', b'm', b'a', b'l', b'l'];
        let envelopes = HashMap::from([
            (base_id, raw_envelope(b"evil")),
            (target_id, delta_envelope(base_id, target.len(), patch)),
        ]);
        assert!(matches!(
            resolve_from(envelopes, target_id, target.len()),
            Err(ContentStoreError::Corrupt { expected, .. }) if expected == base_id
        ));
    }

    #[test]
    fn delta_resolution_rejects_wrong_intermediate_even_when_final_patch_ignores_it() {
        let base_id = ContentId::of(b"base");
        let middle_id = ContentId::of(b"good");
        let target = b"small";
        let target_id = ContentId::of(target);
        let envelopes = HashMap::from([
            (base_id, raw_envelope(b"base")),
            (
                middle_id,
                delta_envelope(base_id, 4, vec![4, 4, 4, b'e', b'v', b'i', b'l']),
            ),
            (
                target_id,
                delta_envelope(middle_id, 5, vec![4, 5, 5, b's', b'm', b'a', b'l', b'l']),
            ),
        ]);
        assert!(matches!(
            resolve_from(envelopes, target_id, target.len()),
            Err(ContentStoreError::Corrupt { expected, .. }) if expected == middle_id
        ));
    }

    #[test]
    fn delta_resolution_verifies_each_link_and_resolves_valid_chain() {
        let base = b"the quick brown fox jumps over the lazy dog".repeat(4);
        let middle = [base.as_slice(), b" middle"].concat();
        let target = [middle.as_slice(), b" target"].concat();
        let base_id = ContentId::of(&base);
        let middle_id = ContentId::of(&middle);
        let target_id = ContentId::of(&target);
        let middle_patch = crate::delta::compute_delta(&base, &middle).unwrap();
        let target_patch = crate::delta::compute_delta(&middle, &target).unwrap();
        let envelopes = HashMap::from([
            (base_id, raw_envelope(&base)),
            (
                middle_id,
                delta_envelope(base_id, middle.len(), middle_patch),
            ),
            (
                target_id,
                delta_envelope(middle_id, target.len(), target_patch),
            ),
        ]);

        assert_eq!(
            resolve_from(envelopes, target_id, target.len()).unwrap(),
            target
        );
    }
}
