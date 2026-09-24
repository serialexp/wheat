//! Reusable authenticated content-generation index and shard reader.
//!
//! The catalog head selects the generation. A store handle retains its small
//! top-level index and loads shard manifests and pack handles on demand. One
//! read resolves a delta chain of at most `MAX_DELTA_CHAIN_DEPTH` bases, each
//! possibly in another shard, so the reader keeps that many shards plus the
//! target open; any more is evicted least-recently-used. A new head replaces
//! the whole reader before any lookup against the new generation.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use wheat_core::{ContentId, PhysicalContentGeneration};

use crate::content_store::MAX_DELTA_CHAIN_DEPTH;
use crate::{
    ContentGenerationEntry, ContentGenerationIndex, ContentStoreError, codec,
    content_generation_shard_manifest_path, content_generation_shard_pack_path,
    read_content_generation_index,
};

/// Shards one chain read can touch: every base plus the target.
const OPEN_SHARDS: usize = MAX_DELTA_CHAIN_DEPTH + 1;

struct OpenShard {
    shard: usize,
    entries: Vec<ContentGenerationEntry>,
    pack: File,
}

pub(super) struct GenerationReader {
    authority: PhysicalContentGeneration,
    generations: PathBuf,
    index: ContentGenerationIndex,
    /// Most recently used last.
    open: Vec<OpenShard>,
}

impl GenerationReader {
    pub(super) fn open(root: &Path, authority: PhysicalContentGeneration) -> Result<Self, String> {
        let generations = root.join("content-generations");
        let index = read_content_generation_index(&generations, authority)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            authority,
            generations,
            index,
            open: Vec::with_capacity(OPEN_SHARDS),
        })
    }

    pub(super) fn matches(&self, authority: PhysicalContentGeneration) -> bool {
        self.authority == authority
    }

    /// Open (or reuse) `shard` and move it to the most-recently-used slot.
    fn shard(&mut self, shard: usize) -> Result<&mut OpenShard, ContentStoreError> {
        if let Some(position) = self.open.iter().position(|open| open.shard == shard) {
            let open = self.open.remove(position);
            self.open.push(open);
        } else {
            let generation = self.authority.generation;
            let encoded = std::fs::read(content_generation_shard_manifest_path(
                &self.generations,
                generation,
                shard,
            ))?;
            let entries = self
                .index
                .decode_shard(shard, &encoded)
                .map_err(|error| ContentStoreError::Io(std::io::Error::other(error.to_string())))?;
            let pack = File::open(content_generation_shard_pack_path(
                &self.generations,
                generation,
                shard,
            ))?;
            if pack.metadata()?.len() != self.index.shards[shard].pack_length {
                return Err(ContentStoreError::Io(std::io::Error::other(
                    "content shard pack length differs from its manifest",
                )));
            }
            if self.open.len() == OPEN_SHARDS {
                self.open.remove(0);
            }
            self.open.push(OpenShard {
                shard,
                entries,
                pack,
            });
        }
        Ok(self.open.last_mut().expect("a shard was just opened"))
    }

    pub(super) fn read(
        &mut self,
        id: ContentId,
    ) -> Result<Option<(codec::EnvelopeHeader, Vec<u8>)>, ContentStoreError> {
        let Some(shard) = self.index.shard_for(id) else {
            return Ok(None);
        };
        let open = self.shard(shard)?;
        let Ok(index) = open.entries.binary_search_by_key(&id, |entry| entry.id) else {
            return Ok(None);
        };
        let entry = &open.entries[index];
        let length = usize::try_from(entry.physical_length).map_err(|_| {
            ContentStoreError::InvalidFormat {
                id,
                reason: "generation envelope length exceeds address space".into(),
            }
        })?;
        if length < codec::ENVELOPE_HEADER_LEN {
            return Err(ContentStoreError::InvalidFormat {
                id,
                reason: "generation envelope differs from manifest".into(),
            });
        }
        open.pack.seek(SeekFrom::Start(entry.pack_offset))?;
        let mut header_bytes = [0; codec::ENVELOPE_HEADER_LEN];
        open.pack.read_exact(&mut header_bytes)?;
        let mut payload = vec![0; length - codec::ENVELOPE_HEADER_LEN];
        open.pack.read_exact(&mut payload)?;
        let mut digest = blake3::Hasher::new();
        digest.update(&header_bytes);
        digest.update(&payload);
        if digest.finalize().as_bytes() != &entry.envelope_digest {
            return Err(ContentStoreError::InvalidFormat {
                id,
                reason: "generation envelope differs from manifest".into(),
            });
        }
        let header = codec::parse_envelope_header(&header_bytes).map_err(|error| {
            ContentStoreError::InvalidFormat {
                id,
                reason: error.to_string(),
            }
        })?;
        Ok(Some((header, payload)))
    }
}
