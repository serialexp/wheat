//! Sharded physical content generations: authenticated types, canonical
//! encoding, and on-disk layout.
//!
//! A generation is split in ContentId order into shards of exactly
//! [`CONTENT_GENERATION_SHARD_ENTRIES`] entries (the last may hold fewer).
//! Each shard has its own manifest and pack; a small top-level index names
//! every shard by first ID, size, and manifest digest, and the catalog names
//! the index by its digest. A reader loads the index and then only the shards
//! it touches. A delta base may live in any shard.
//!
//! Layout under `content-generations/`:
//! - `{generation}-{index digest}.manifest`: the top-level index;
//! - `{generation}/shard-{n}.manifest` and `{generation}/shard-{n}.pack`.

use std::io;
use std::path::{Path, PathBuf};

use wheat_core::{
    ContentGenerationWireEntry, ContentGenerationWireIndex, ContentGenerationWireShard,
    ContentGenerationWireShardRef, ContentId, PhysicalContentGeneration,
    content_generation_fingerprint, content_generation_index_digest,
    decode_content_generation_index, decode_content_generation_shard,
    encode_content_generation_index, encode_content_generation_shard,
};

pub use wheat_core::CONTENT_GENERATION_SHARD_ENTRIES;

use crate::ContentRepackError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationEntry {
    pub id: ContentId,
    pub envelope_digest: [u8; 32],
    /// Offset in this entry's shard pack.
    pub pack_offset: u64,
    pub physical_length: u64,
    pub logical_length: u64,
    pub delta_base: Option<ContentId>,
    pub delta_depth: u8,
}

/// One shard as the top-level index names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentGenerationShardRef {
    pub first_id: ContentId,
    pub entry_count: u32,
    pub pack_length: u64,
    /// BLAKE3 digest of the shard's encoded manifest.
    pub manifest: [u8; 32],
}

/// Authenticated top-level index: what a reader needs to find one shard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationIndex {
    pub generation: u64,
    pub entry_count: u64,
    pub shards: Vec<ContentGenerationShardRef>,
    /// Generation-independent representation fingerprint used for no-op detection.
    pub content_fingerprint: [u8; 32],
    /// Digest over the generation number, fingerprint, and shard references.
    pub manifest_digest: [u8; 32],
}

/// A complete generation with every entry in ID order, for planning, staging,
/// and whole-generation validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationManifest {
    pub generation: u64,
    pub entries: Vec<ContentGenerationEntry>,
    /// Generation-independent representation fingerprint used for no-op detection.
    pub content_fingerprint: [u8; 32],
    /// The digest its [`ContentGenerationIndex`] carries.
    pub manifest_digest: [u8; 32],
}

fn invalid(reason: &str) -> ContentRepackError {
    ContentRepackError::InvalidManifest(reason.into())
}

impl ContentGenerationIndex {
    pub fn encoded(&self) -> Result<Vec<u8>, ContentRepackError> {
        encode_content_generation_index(&ContentGenerationWireIndex {
            generation: self.generation,
            entry_count: self.entry_count,
            shards: self.shards.iter().map(shard_ref_wire).collect(),
            content_fingerprint: self.content_fingerprint,
            manifest_digest: self.manifest_digest,
        })
        .map_err(|error| invalid(&error.to_string()))
    }

    /// Decode and authenticate an index: canonical bytes, full shards, first
    /// IDs strictly increasing, and a matching manifest digest.
    pub fn decode(encoded: &[u8]) -> Result<Self, ContentRepackError> {
        let wire = decode_content_generation_index(encoded)
            .map_err(|error| invalid(&error.to_string()))?;
        let index = Self {
            generation: wire.generation,
            entry_count: wire.entry_count,
            shards: wire
                .shards
                .into_iter()
                .map(|shard| ContentGenerationShardRef {
                    first_id: shard.first_id,
                    entry_count: shard.entry_count,
                    pack_length: shard.pack_length,
                    manifest: shard.manifest,
                })
                .collect(),
            content_fingerprint: wire.content_fingerprint,
            manifest_digest: wire.manifest_digest,
        };
        if index
            .shards
            .windows(2)
            .any(|pair| pair[0].first_id >= pair[1].first_id)
        {
            return Err(invalid("shards are not strictly ordered"));
        }
        if index_digest(
            index.generation,
            index.content_fingerprint,
            index.entry_count,
            &index.shards,
        )? != index.manifest_digest
        {
            return Err(invalid("manifest digest differs"));
        }
        Ok(index)
    }

    /// The shard whose ID range holds `id`, if any.
    #[must_use]
    pub fn shard_for(&self, id: ContentId) -> Option<usize> {
        self.shards
            .partition_point(|shard| shard.first_id <= id)
            .checked_sub(1)
    }

    /// Decode and authenticate shard `shard` from its manifest bytes: its
    /// digest, generation, number, size, ID range, order, and pack length all
    /// match the index.
    pub fn decode_shard(
        &self,
        shard: usize,
        encoded: &[u8],
    ) -> Result<Vec<ContentGenerationEntry>, ContentRepackError> {
        let reference = self
            .shards
            .get(shard)
            .ok_or_else(|| invalid("shard is outside the index"))?;
        if blake3::hash(encoded).as_bytes() != &reference.manifest {
            return Err(invalid("shard manifest digest differs"));
        }
        let wire = decode_content_generation_shard(encoded)
            .map_err(|error| invalid(&error.to_string()))?;
        if wire.generation != self.generation
            || usize::try_from(wire.shard).ok() != Some(shard)
            || wire.entries.len() != reference.entry_count as usize
        {
            return Err(invalid("shard manifest differs from its index entry"));
        }
        let entries = wire
            .entries
            .into_iter()
            .map(entry_domain)
            .collect::<Vec<_>>();
        if entries.first().map(|entry| entry.id) != Some(reference.first_id)
            || entries.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || self
                .shards
                .get(shard + 1)
                .is_some_and(|next| entries.last().is_some_and(|last| last.id >= next.first_id))
        {
            return Err(invalid("entries are not strictly ordered"));
        }
        if pack_length(&entries)? != reference.pack_length {
            return Err(invalid("shard pack length differs from its entries"));
        }
        Ok(entries)
    }
}

impl ContentGenerationManifest {
    /// Seal `entries` (which must be strictly ordered by ID) into an
    /// authenticated manifest for `generation`.
    pub fn new(
        generation: u64,
        entries: Vec<ContentGenerationEntry>,
    ) -> Result<Self, ContentRepackError> {
        let content_fingerprint = fingerprint_entries(&entries)?;
        let mut manifest = Self {
            generation,
            entries,
            content_fingerprint,
            manifest_digest: [0; 32],
        };
        manifest.manifest_digest = manifest.index()?.manifest_digest;
        manifest.verify()?;
        Ok(manifest)
    }

    /// Authenticate the manifest and reject non-canonical entry ordering.
    pub fn verify(&self) -> Result<(), ContentRepackError> {
        if self.entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(invalid("entries are not strictly ordered"));
        }
        if fingerprint_entries(&self.entries)? != self.content_fingerprint {
            return Err(invalid("content fingerprint differs"));
        }
        if self.index()?.manifest_digest != self.manifest_digest {
            return Err(invalid("manifest digest differs"));
        }
        Ok(())
    }

    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.entries
            .len()
            .div_ceil(CONTENT_GENERATION_SHARD_ENTRIES)
    }

    /// The entries of shard `shard`, in ID order.
    #[must_use]
    pub fn shard_entries(&self, shard: usize) -> &[ContentGenerationEntry] {
        let start = shard.saturating_mul(CONTENT_GENERATION_SHARD_ENTRIES);
        let end = start
            .saturating_add(CONTENT_GENERATION_SHARD_ENTRIES)
            .min(self.entries.len());
        self.entries.get(start..end).unwrap_or(&[])
    }

    /// The shard holding the entry at `entry_index` of [`Self::entries`].
    #[must_use]
    pub fn shard_of(entry_index: usize) -> usize {
        entry_index / CONTENT_GENERATION_SHARD_ENTRIES
    }

    /// Encode shard `shard`'s manifest.
    pub fn encode_shard(&self, shard: usize) -> Result<Vec<u8>, ContentRepackError> {
        encode_content_generation_shard(&ContentGenerationWireShard {
            generation: self.generation,
            shard: u32::try_from(shard)
                .map_err(|_| ContentRepackError::ArithmeticOverflow("shard number"))?,
            entries: self.shard_entries(shard).iter().map(entry_wire).collect(),
        })
        .map_err(|error| invalid(&error.to_string()))
    }

    /// The top-level index naming every shard. Hashes each encoded shard.
    pub fn index(&self) -> Result<ContentGenerationIndex, ContentRepackError> {
        let mut shards = Vec::with_capacity(self.shard_count());
        for shard in 0..self.shard_count() {
            let entries = self.shard_entries(shard);
            shards.push(ContentGenerationShardRef {
                first_id: entries[0].id,
                entry_count: u32::try_from(entries.len())
                    .map_err(|_| ContentRepackError::ArithmeticOverflow("shard entry count"))?,
                pack_length: pack_length(entries)?,
                manifest: *blake3::hash(&self.encode_shard(shard)?).as_bytes(),
            });
        }
        let entry_count = u64::try_from(self.entries.len())
            .map_err(|_| ContentRepackError::ArithmeticOverflow("entry count"))?;
        Ok(ContentGenerationIndex {
            generation: self.generation,
            entry_count,
            manifest_digest: index_digest(
                self.generation,
                self.content_fingerprint,
                entry_count,
                &shards,
            )?,
            shards,
            content_fingerprint: self.content_fingerprint,
        })
    }

    /// Decode a complete generation: the index, then every shard in order via
    /// `read_shard`, each authenticated against the index.
    pub fn decode(
        index: &[u8],
        mut read_shard: impl FnMut(usize) -> io::Result<Vec<u8>>,
    ) -> Result<Self, ContentRepackError> {
        let index = ContentGenerationIndex::decode(index)?;
        let capacity = usize::try_from(index.entry_count)
            .map_err(|_| ContentRepackError::ArithmeticOverflow("entry count"))?;
        let mut entries = Vec::with_capacity(capacity);
        for shard in 0..index.shards.len() {
            entries.extend(index.decode_shard(shard, &read_shard(shard)?)?);
        }
        let manifest = Self {
            generation: index.generation,
            entries,
            content_fingerprint: index.content_fingerprint,
            manifest_digest: index.manifest_digest,
        };
        if fingerprint_entries(&manifest.entries)? != manifest.content_fingerprint {
            return Err(invalid("content fingerprint differs"));
        }
        Ok(manifest)
    }
}

/// Path of generation `generation`'s top-level index named by `digest`.
#[must_use]
pub fn content_generation_index_path(
    generations: &Path,
    generation: u64,
    digest: &[u8; 32],
) -> PathBuf {
    generations.join(format!("{generation}-{}.manifest", hex::encode(digest)))
}

/// Path of one shard manifest.
#[must_use]
pub fn content_generation_shard_manifest_path(
    generations: &Path,
    generation: u64,
    shard: usize,
) -> PathBuf {
    generations
        .join(generation.to_string())
        .join(format!("shard-{shard:06}.manifest"))
}

/// Path of one shard pack.
#[must_use]
pub fn content_generation_shard_pack_path(
    generations: &Path,
    generation: u64,
    shard: usize,
) -> PathBuf {
    generations
        .join(generation.to_string())
        .join(format!("shard-{shard:06}.pack"))
}

/// Read and authenticate the index `authority` selects, without any shard.
pub fn read_content_generation_index(
    generations: &Path,
    authority: PhysicalContentGeneration,
) -> Result<ContentGenerationIndex, ContentRepackError> {
    let encoded = std::fs::read(content_generation_index_path(
        generations,
        authority.generation,
        &authority.manifest,
    ))?;
    if blake3::hash(&encoded).as_bytes() != &authority.manifest {
        return Err(invalid("index digest differs from its authority"));
    }
    let index = ContentGenerationIndex::decode(&encoded)?;
    if index.generation != authority.generation {
        return Err(invalid("index names another generation"));
    }
    Ok(index)
}

/// Read and authenticate the complete generation `authority` selects.
pub fn read_content_generation(
    generations: &Path,
    authority: PhysicalContentGeneration,
) -> Result<ContentGenerationManifest, ContentRepackError> {
    let path =
        content_generation_index_path(generations, authority.generation, &authority.manifest);
    let encoded = std::fs::read(path)?;
    if blake3::hash(&encoded).as_bytes() != &authority.manifest {
        return Err(invalid("index digest differs from its authority"));
    }
    let manifest = ContentGenerationManifest::decode(&encoded, |shard| {
        std::fs::read(content_generation_shard_manifest_path(
            generations,
            authority.generation,
            shard,
        ))
    })?;
    if manifest.generation != authority.generation {
        return Err(invalid("index names another generation"));
    }
    Ok(manifest)
}

fn pack_length(entries: &[ContentGenerationEntry]) -> Result<u64, ContentRepackError> {
    entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.physical_length)
            .ok_or(ContentRepackError::ArithmeticOverflow("shard pack length"))
    })
}

fn index_digest(
    generation: u64,
    content_fingerprint: [u8; 32],
    entry_count: u64,
    shards: &[ContentGenerationShardRef],
) -> Result<[u8; 32], ContentRepackError> {
    content_generation_index_digest(
        generation,
        content_fingerprint,
        entry_count,
        shards.iter().map(shard_ref_wire),
    )
    .map_err(|error| invalid(&error.to_string()))
}

/// Generation-independent fingerprint of every entry, including where each
/// sits in its shard pack.
pub(crate) fn fingerprint_entries(
    entries: &[ContentGenerationEntry],
) -> Result<[u8; 32], ContentRepackError> {
    content_generation_fingerprint(entries.iter().map(entry_wire))
        .map_err(|error| invalid(&error.to_string()))
}

fn shard_ref_wire(shard: &ContentGenerationShardRef) -> ContentGenerationWireShardRef {
    ContentGenerationWireShardRef {
        first_id: shard.first_id,
        entry_count: shard.entry_count,
        pack_length: shard.pack_length,
        manifest: shard.manifest,
    }
}

fn entry_wire(entry: &ContentGenerationEntry) -> ContentGenerationWireEntry {
    ContentGenerationWireEntry {
        id: entry.id,
        envelope_digest: entry.envelope_digest,
        pack_offset: entry.pack_offset,
        physical_length: entry.physical_length,
        logical_length: entry.logical_length,
        delta_base: entry.delta_base,
        delta_depth: entry.delta_depth,
    }
}

fn entry_domain(entry: ContentGenerationWireEntry) -> ContentGenerationEntry {
    ContentGenerationEntry {
        id: entry.id,
        envelope_digest: entry.envelope_digest,
        pack_offset: entry.pack_offset,
        physical_length: entry.physical_length,
        logical_length: entry.logical_length,
        delta_base: entry.delta_base,
        delta_depth: entry.delta_depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> ContentGenerationEntry {
        ContentGenerationEntry {
            id: ContentId::of(&n.to_be_bytes()),
            envelope_digest: [n as u8; 32],
            pack_offset: 0,
            physical_length: 100,
            logical_length: 200,
            delta_base: None,
            delta_depth: 0,
        }
    }

    /// `count` entries in ID order, each shard's pack laid out from zero.
    fn manifest(count: u64) -> ContentGenerationManifest {
        let mut entries = (0..count).map(entry).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.id);
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.pack_offset = (index % CONTENT_GENERATION_SHARD_ENTRIES) as u64 * 100;
        }
        ContentGenerationManifest::new(3, entries).unwrap()
    }

    fn shards(manifest: &ContentGenerationManifest) -> Vec<Vec<u8>> {
        (0..manifest.shard_count())
            .map(|shard| manifest.encode_shard(shard).unwrap())
            .collect()
    }

    #[test]
    fn entries_split_into_full_shards_and_one_partial_tail() {
        let count = 2 * CONTENT_GENERATION_SHARD_ENTRIES as u64 + 5;
        let manifest = manifest(count);
        let index = manifest.index().unwrap();
        assert_eq!(index.entry_count, count);
        let sizes = index
            .shards
            .iter()
            .map(|shard| shard.entry_count)
            .collect::<Vec<_>>();
        let full = CONTENT_GENERATION_SHARD_ENTRIES as u32;
        assert_eq!(sizes, [full, full, 5]);
        for shard in &index.shards {
            assert_eq!(shard.pack_length, u64::from(shard.entry_count) * 100);
        }
        let encoded = shards(&manifest);
        let decoded = ContentGenerationManifest::decode(&index.encoded().unwrap(), |shard| {
            Ok(encoded[shard].clone())
        })
        .unwrap();
        assert_eq!(decoded, manifest);

        // Every ID maps to the shard that holds it; IDs below the first map to none.
        for (position, entry) in manifest.entries.iter().enumerate() {
            assert_eq!(
                index.shard_for(entry.id),
                Some(ContentGenerationManifest::shard_of(position))
            );
        }
        let below = ContentId::from_digest([0; 32]);
        assert!(below < manifest.entries[0].id);
        assert_eq!(index.shard_for(below), None);
    }

    #[test]
    fn empty_generation_has_no_shards() {
        let manifest = manifest(0);
        let index = manifest.index().unwrap();
        assert!(index.shards.is_empty());
        let decoded =
            ContentGenerationManifest::decode(&index.encoded().unwrap(), |_| unreachable!())
                .unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(index.shard_for(ContentId::of(b"any")), None);
    }

    #[test]
    fn index_digest_binds_generation_and_every_shard_reference() {
        let manifest = manifest(CONTENT_GENERATION_SHARD_ENTRIES as u64 + 1);
        let index = manifest.index().unwrap();
        let forged = [
            ContentGenerationIndex {
                generation: 4,
                ..index.clone()
            },
            {
                let mut forged = index.clone();
                forged.shards[1].pack_length += 1;
                forged
            },
            {
                let mut forged = index.clone();
                forged.shards[1].manifest[0] ^= 1;
                forged
            },
            {
                let mut forged = index.clone();
                forged.content_fingerprint[0] ^= 1;
                forged
            },
        ];
        for forged in forged {
            let error = ContentGenerationIndex::decode(&forged.encoded().unwrap()).unwrap_err();
            assert!(error.to_string().contains("digest"), "{error}");
        }
    }

    #[test]
    fn shards_are_bound_to_their_index_position_generation_and_pack() {
        let manifest = manifest(CONTENT_GENERATION_SHARD_ENTRIES as u64 + 3);
        let index = manifest.index().unwrap();
        let encoded = shards(&manifest);
        assert_eq!(index.decode_shard(1, &encoded[1]).unwrap().len(), 3);
        // A valid shard in the wrong position fails its digest check.
        assert!(index.decode_shard(0, &encoded[1]).is_err());
        assert!(index.decode_shard(2, &encoded[1]).is_err());

        // Past the digest (as a forger who rewrites the index would be), the
        // shard's own header and entries must still agree with the index.
        let reindexed = |bytes: &[u8], edit: &dyn Fn(&mut ContentGenerationShardRef)| {
            let mut forged = index.clone();
            forged.shards[1].manifest = *blake3::hash(bytes).as_bytes();
            edit(&mut forged.shards[1]);
            forged
        };
        let other_generation = ContentGenerationManifest::new(9, manifest.entries.clone())
            .unwrap()
            .encode_shard(1)
            .unwrap();
        // Shard 1's entries under shard number 0.
        let other_position = encode_content_generation_shard(&ContentGenerationWireShard {
            generation: manifest.generation,
            shard: 0,
            entries: manifest.shard_entries(1).iter().map(entry_wire).collect(),
        })
        .unwrap();
        type Edit = fn(&mut ContentGenerationShardRef);
        let cases: [(&str, &[u8], Edit); 5] = [
            ("generation", &other_generation, |_| {}),
            ("shard number", &other_position, |_| {}),
            ("entry count", &encoded[1], |shard| shard.entry_count += 1),
            ("pack length", &encoded[1], |shard| shard.pack_length += 1),
            ("first ID", &encoded[1], |shard| {
                shard.first_id = ContentId::from_digest([0xff; 32]);
            }),
        ];
        for (label, bytes, edit) in cases {
            assert!(
                reindexed(bytes, &edit).decode_shard(1, bytes).is_err(),
                "{label}"
            );
        }
    }

    #[test]
    fn unordered_shards_are_rejected() {
        let manifest = manifest(2 * CONTENT_GENERATION_SHARD_ENTRIES as u64);
        let mut index = manifest.index().unwrap();
        index.shards.swap(0, 1);
        index.manifest_digest = index_digest(
            index.generation,
            index.content_fingerprint,
            index.entry_count,
            &index.shards,
        )
        .unwrap();
        let error = ContentGenerationIndex::decode(&index.encoded().unwrap()).unwrap_err();
        assert!(error.to_string().contains("strictly ordered"), "{error}");
    }
}
