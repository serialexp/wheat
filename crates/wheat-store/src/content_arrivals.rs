//! Receiving long blobs. A sender sends a blob over [`CHUNKED_CONTENT_THRESHOLD`] as
//! its manifest; the receiver holds the manifests of one answer, asks in rounds for
//! the parts and then the chunks it lacks, and once it has them all checks each
//! blob's ID and stores its manifest ([`crate::RepositoryStage::put_manifest`]).
//! See `docs/design/wheat-large-content.md`.
//!
//! [`CHUNKED_CONTENT_THRESHOLD`]: wheat_core::CHUNKED_CONTENT_THRESHOLD

use std::collections::HashSet;

use thiserror::Error;
use wheat_core::{
    ContentChunkManifestRecord, ContentId, ContentManifestEntries, StoreRecordError,
    decode_content_chunk_manifest,
};

use crate::{ContentStore, ContentStoreError, content_manifest::ManifestChunks};

/// Why a received manifest is refused.
#[derive(Debug, Error)]
pub enum ManifestArrivalError {
    #[error("the manifest of {id} is invalid: {error}")]
    Invalid {
        id: ContentId,
        error: StoreRecordError,
    },
    #[error("an answer carried more than {limit} bytes of manifests")]
    OverLimit { limit: usize },
}

/// The manifests of one answer, held until the receiver has every part and chunk
/// they name. Holds at most `limit` bytes of encoded manifests, and the chunk list
/// of one part while it asks.
#[derive(Debug)]
pub struct ManifestArrivals {
    limit: usize,
    held: Vec<(ContentId, ContentChunkManifestRecord)>,
    held_bytes: usize,
    asking: Asking,
}

/// How far the asking has got through the held manifests.
#[derive(Debug)]
enum Asking {
    /// The next part to look up: the manifest's index and the part's.
    Parts { manifest: usize, part: usize },
    /// The next chunk to look up: the manifest's index and its walk, once begun.
    Chunks {
        manifest: usize,
        walk: Option<ManifestChunks>,
    },
}

impl Asking {
    const START: Self = Self::Parts {
        manifest: 0,
        part: 0,
    };
}

/// What a look for candidates ended with.
enum Progress {
    /// It found as many as wanted; there may be more.
    More,
    /// It has looked up every part; the chunks come next.
    PartsDone,
    /// It has looked up every part and chunk.
    Done,
}

impl ManifestArrivals {
    /// Holds up to `limit` bytes of encoded manifests; one manifest is always taken.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            held: Vec::new(),
            held_bytes: 0,
            asking: Asking::START,
        }
    }

    /// Whether no manifest is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// The encoded bytes of the manifests held.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.held_bytes
    }

    /// Holds the manifest of `id`, as received. It must be valid, and with the
    /// manifests already held fit the limit, unless it is the first.
    pub fn hold(&mut self, id: ContentId, encoded: &[u8]) -> Result<(), ManifestArrivalError> {
        let total = self.held_bytes.saturating_add(encoded.len());
        if !self.held.is_empty() && total > self.limit {
            return Err(ManifestArrivalError::OverLimit { limit: self.limit });
        }
        let manifest = decode_content_chunk_manifest(encoded)
            .map_err(|error| ManifestArrivalError::Invalid { id, error })?;
        self.held.push((id, manifest));
        self.held_bytes = total;
        Ok(())
    }

    /// The next at most `max` parts or chunks of the held manifests that `content`
    /// lacks, each once: every missing part before any chunk, since a part says
    /// which chunks to look for. Empty once `content` has them all. Each call goes on
    /// from where the last stopped, so the caller stores a round before asking for
    /// the next.
    pub fn next_round(
        &mut self,
        content: &ContentStore,
        max: usize,
    ) -> Result<Vec<ContentId>, ContentStoreError> {
        assert!(max != 0, "a round asks for at least one ID");
        let mut round = Vec::new();
        let mut asked = HashSet::new();
        let mut candidates = Vec::new();
        loop {
            candidates.clear();
            let wanted = max - round.len();
            let progress = self.candidates(content, wanted, &mut candidates)?;
            if !candidates.is_empty() {
                let existing = content.existing_ids(&candidates)?;
                for &id in &candidates {
                    if !existing.contains(&id) && asked.insert(id) {
                        round.push(id);
                    }
                }
            }
            let done = match progress {
                Progress::More => round.len() == max,
                // The parts asked for must arrive before the walk reads them.
                Progress::PartsDone => !round.is_empty(),
                Progress::Done => true,
            };
            if done {
                return Ok(round);
            }
        }
    }

    /// Up to `wanted` parts or chunks to look up, from where the asking stopped.
    fn candidates(
        &mut self,
        content: &ContentStore,
        wanted: usize,
        out: &mut Vec<ContentId>,
    ) -> Result<Progress, ContentStoreError> {
        while out.len() < wanted {
            match &mut self.asking {
                Asking::Parts { manifest, part } => {
                    let Some((_, held)) = self.held.get(*manifest) else {
                        self.asking = Asking::Chunks {
                            manifest: 0,
                            walk: None,
                        };
                        return Ok(Progress::PartsDone);
                    };
                    let parts = match &held.entries {
                        ContentManifestEntries::Parts(parts) => parts.as_slice(),
                        ContentManifestEntries::Chunks(_) => &[],
                    };
                    match parts.get(*part) {
                        Some(next) => {
                            out.push(next.id);
                            *part += 1;
                        }
                        None => {
                            *manifest += 1;
                            *part = 0;
                        }
                    }
                }
                Asking::Chunks { manifest, walk } => {
                    let Some((id, held)) = self.held.get(*manifest) else {
                        return Ok(Progress::Done);
                    };
                    let chunks = walk.get_or_insert_with(|| ManifestChunks::new(*id, held.clone()));
                    match chunks.next(|part, max| content.get_bounded(part, max))? {
                        Some(chunk) => out.push(chunk.id),
                        None => {
                            *manifest += 1;
                            *walk = None;
                        }
                    }
                }
            }
        }
        Ok(Progress::More)
    }

    /// Takes the held manifests, once [`Self::next_round`] has come back empty,
    /// leaving none held.
    pub fn take(&mut self) -> Vec<(ContentId, ContentChunkManifestRecord)> {
        self.held_bytes = 0;
        self.asking = Asking::START;
        std::mem::take(&mut self.held)
    }
}

impl ContentStore {
    /// The manifest of `id` when it is stored as chunks; `None` for a blob stored
    /// whole.
    pub fn chunk_manifest(
        &self,
        id: ContentId,
    ) -> Result<Option<ContentChunkManifestRecord>, ContentStoreError> {
        if !crate::content_store::is_chunked_length(self.logical_length(id)?) {
            return Ok(None);
        }
        match self.stored(id)? {
            crate::content_store::StoredContent::Envelope(header, payload)
                if header.codec == crate::codec::Codec::Chunked =>
            {
                crate::content_stream::chunk_manifest(id, header, payload).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Checks that the chunks `manifest` names, all stored here, hold the bytes of
    /// `id`: reads them in order through a hasher, a chunk at a time.
    pub fn check_manifest(
        &self,
        id: ContentId,
        manifest: &ContentChunkManifestRecord,
    ) -> Result<(), ContentStoreError> {
        let mut hasher = ContentId::hasher();
        let mut chunks = ManifestChunks::new(id, manifest.clone());
        while let Some(chunk) = chunks.next(|part, max| self.get_bounded(part, max))? {
            let bytes = self.get_bounded(chunk.id, chunk.length as usize)?;
            if bytes.len() != chunk.length as usize {
                return Err(ContentStoreError::InvalidFormat {
                    id,
                    reason: format!("chunk {} is not as long as its manifest says", chunk.id),
                });
            }
            hasher.update(&bytes);
        }
        let actual = ContentId::from_digest(*hasher.finalize().as_bytes());
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        Ok(())
    }
}
