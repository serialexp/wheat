//! Reusable authenticated content-generation index and pack reader.
//!
//! The catalog head selects the generation. A store handle retains only the
//! currently selected immutable manifest and an open pack handle; a new head
//! replaces both together before any lookup against the new generation.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use wheat_core::{ContentId, PhysicalContentGeneration};

use crate::{ContentGenerationManifest, ContentStoreError, codec};

use super::store::read_content_manifest;

pub(super) struct GenerationReader {
    authority: PhysicalContentGeneration,
    manifest: ContentGenerationManifest,
    pack: File,
}

impl GenerationReader {
    pub(super) fn open(root: &Path, authority: PhysicalContentGeneration) -> Result<Self, String> {
        let manifest = read_content_manifest(root, authority)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "missing selected content generation".to_owned())?;
        let pack = File::open(
            root.join("content-generations")
                .join(authority.generation.to_string())
                .join("content.pack"),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            authority,
            manifest,
            pack,
        })
    }

    pub(super) fn matches(&self, authority: PhysicalContentGeneration) -> bool {
        self.authority == authority
    }

    pub(super) fn read(
        &mut self,
        id: ContentId,
    ) -> Result<Option<(codec::EnvelopeHeader, Vec<u8>)>, ContentStoreError> {
        let Ok(index) = self
            .manifest
            .entries
            .binary_search_by_key(&id, |entry| entry.id)
        else {
            return Ok(None);
        };
        let entry = &self.manifest.entries[index];
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
        self.pack.seek(SeekFrom::Start(entry.pack_offset))?;
        let mut header_bytes = [0; codec::ENVELOPE_HEADER_LEN];
        self.pack.read_exact(&mut header_bytes)?;
        let mut payload = vec![0; length - codec::ENVELOPE_HEADER_LEN];
        self.pack.read_exact(&mut payload)?;
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
