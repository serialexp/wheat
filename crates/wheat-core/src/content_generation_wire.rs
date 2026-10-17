//! Canonical BinSchema representation of physical content-repack manifests.
//!
//! A generation is a small top-level index naming ID-range shards; each shard
//! has its own manifest and pack. These records describe replaceable storage
//! representation rather than logical Wheat object identity, so they
//! intentionally remain outside `WheatObject`.

use thiserror::Error;
use wheat_binschema::generated as wire;

use crate::ContentId;

/// Entries per shard. Entries are split in ID order: every shard but the last
/// holds exactly this many, the last holds at least one. The index has no
/// entry cap; a reader decodes at most one shard of this size at a time.
pub const CONTENT_GENERATION_SHARD_ENTRIES: usize = 16_384;

/// One content envelope's physical representation within its shard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationWireEntry {
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
pub struct ContentGenerationWireShardRef {
    pub first_id: ContentId,
    pub entry_count: u32,
    pub pack_length: u64,
    /// BLAKE3 digest of the shard's encoded manifest.
    pub manifest: [u8; 32],
}

/// Top-level index of one complete physical content generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationWireIndex {
    pub generation: u64,
    pub entry_count: u64,
    pub shards: Vec<ContentGenerationWireShardRef>,
    /// Generation-independent representation fingerprint used for no-op detection.
    pub content_fingerprint: [u8; 32],
    /// Digest over the generation number, fingerprint, and shard references.
    pub manifest_digest: [u8; 32],
}

/// The entries of one shard, in ID order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentGenerationWireShard {
    pub generation: u64,
    pub shard: u32,
    pub entries: Vec<ContentGenerationWireEntry>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ContentGenerationWireError {
    #[error(
        "content generation shard has too many entries: {actual} > {CONTENT_GENERATION_SHARD_ENTRIES}"
    )]
    TooManyEntries { actual: usize },
    #[error("content generation index does not split {entries} entries into full shards")]
    InvalidShardLayout { entries: u64 },
    #[error("content generation index names more shards than a u32 counts")]
    TooManyShards,
    #[error("content generation manifest does not decode: {0}")]
    Decode(String),
    #[error("content generation manifest is not exactly and canonically encoded")]
    NonCanonicalEncoding,
    #[error("content generation manifest contains an invalid content-id algorithm")]
    InvalidContentIdAlgorithm,
    #[error("content generation manifest digest is not 32 bytes")]
    InvalidDigest,
}

/// Checks that `counts` split `entries` in ID order into full shards: every
/// shard but the last holds exactly [`CONTENT_GENERATION_SHARD_ENTRIES`], the
/// last holds at least one, and an empty generation has no shards.
fn check_shard_layout(
    entries: u64,
    counts: impl ExactSizeIterator<Item = u32>,
) -> Result<(), ContentGenerationWireError> {
    let invalid = ContentGenerationWireError::InvalidShardLayout { entries };
    let full = CONTENT_GENERATION_SHARD_ENTRIES as u64;
    let expected_shards = entries.div_ceil(full);
    if counts.len() as u64 != expected_shards {
        return Err(invalid);
    }
    let mut remaining = entries;
    for count in counts {
        let expected = remaining.min(full);
        if u64::from(count) != expected {
            return Err(invalid);
        }
        remaining -= expected;
    }
    Ok(())
}

/// Encode a generation's top-level index.
pub fn encode_content_generation_index(
    index: &ContentGenerationWireIndex,
) -> Result<Vec<u8>, ContentGenerationWireError> {
    check_shard_layout(
        index.entry_count,
        index.shards.iter().map(|shard| shard.entry_count),
    )?;
    wire::ContentGenerationManifestV1Input {
        generation: index.generation,
        entry_count: index.entry_count,
        shards: index.shards.iter().map(shard_ref_wire).collect(),
        content_fingerprint: digest_wire(index.content_fingerprint),
        manifest_digest: digest_wire(index.manifest_digest),
    }
    .encode()
    .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))
}

/// Decode only a bounded, canonical top-level index whose shard counts split
/// its entry count into full shards.
pub fn decode_content_generation_index(
    bytes: &[u8],
) -> Result<ContentGenerationWireIndex, ContentGenerationWireError> {
    let index = wire::ContentGenerationManifestV1::decode(bytes)
        .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
    let canonical = index
        .encode()
        .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
    if canonical != bytes {
        return Err(ContentGenerationWireError::NonCanonicalEncoding);
    }
    check_shard_layout(
        index.entry_count,
        index.shards.iter().map(|shard| shard.entry_count),
    )?;
    Ok(ContentGenerationWireIndex {
        generation: index.generation,
        entry_count: index.entry_count,
        shards: index
            .shards
            .into_iter()
            .map(shard_ref_domain)
            .collect::<Result<_, _>>()?,
        content_fingerprint: digest_domain(index.content_fingerprint)?,
        manifest_digest: digest_domain(index.manifest_digest)?,
    })
}

/// Encode one shard manifest.
pub fn encode_content_generation_shard(
    shard: &ContentGenerationWireShard,
) -> Result<Vec<u8>, ContentGenerationWireError> {
    if shard.entries.len() > CONTENT_GENERATION_SHARD_ENTRIES {
        return Err(ContentGenerationWireError::TooManyEntries {
            actual: shard.entries.len(),
        });
    }
    wire::ContentGenerationShardV1Input {
        generation: shard.generation,
        shard: shard.shard,
        entries: shard.entries.iter().map(entry_wire).collect(),
    }
    .encode()
    .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))
}

/// Decode only a bounded, canonical shard manifest.
pub fn decode_content_generation_shard(
    bytes: &[u8],
) -> Result<ContentGenerationWireShard, ContentGenerationWireError> {
    let shard = wire::ContentGenerationShardV1::decode(bytes)
        .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
    if shard.entries.len() > CONTENT_GENERATION_SHARD_ENTRIES {
        return Err(ContentGenerationWireError::TooManyEntries {
            actual: shard.entries.len(),
        });
    }
    let canonical = shard
        .encode()
        .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
    if canonical != bytes {
        return Err(ContentGenerationWireError::NonCanonicalEncoding);
    }
    Ok(ContentGenerationWireShard {
        generation: shard.generation,
        shard: shard.shard,
        entries: shard
            .entries
            .into_iter()
            .map(entry_domain)
            .collect::<Result<_, _>>()?,
    })
}

/// The digest an index's `manifest_digest` carries: its generation number,
/// fingerprint, entry count, and shard references.
pub fn content_generation_index_digest(
    generation: u64,
    content_fingerprint: [u8; 32],
    entry_count: u64,
    shards: impl ExactSizeIterator<Item = ContentGenerationWireShardRef>,
) -> Result<[u8; 32], ContentGenerationWireError> {
    // The generated encoder writes the shard count as `len() as u32`.
    u32::try_from(shards.len()).map_err(|_| ContentGenerationWireError::TooManyShards)?;
    let input = wire::ContentGenerationIndexDigestInputV1 {
        generation,
        content_fingerprint: digest_wire(content_fingerprint),
        entry_count,
        shards: shards.map(|shard| shard_ref_wire(&shard)).collect(),
    }
    .encode()
    .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"wheat-content-generation-index-v1\0");
    hash.update(&input);
    Ok(*hash.finalize().as_bytes())
}

/// Generation-independent fingerprint of `entries`, in the order given: every
/// entry's canonical encoding, which says where each sits in its shard pack.
/// An encoding is self-delimiting, so the sequence needs no count.
pub fn content_generation_fingerprint(
    entries: impl IntoIterator<Item = ContentGenerationWireEntry>,
) -> Result<[u8; 32], ContentGenerationWireError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"wheat-content-generation-v1\0");
    let mut scratch = EntryScratch::default();
    for entry in entries {
        hash.update(scratch.encode(&entry)?);
    }
    Ok(*hash.finalize().as_bytes())
}

/// One wire entry and one encoder reused for every entry, so fingerprinting a
/// generation of millions of entries allocates nothing per entry.
struct EntryScratch {
    entry: wire::ContentGenerationEntryV1,
    /// The delta base's buffers while the current entry has none.
    spare_base: Option<wire::ContentId>,
    encoder: wheat_binschema::BitStreamEncoder,
}

impl Default for EntryScratch {
    fn default() -> Self {
        let zero = ContentId::from_digest([0; 32]);
        Self {
            entry: wire::ContentGenerationEntryV1 {
                id: content_id_wire(zero),
                envelope_digest: digest_wire([0; 32]),
                pack_offset: 0,
                physical_length: 0,
                logical_length: 0,
                delta_base: None,
                delta_depth: 0,
            },
            spare_base: Some(content_id_wire(zero)),
            encoder: wheat_binschema::BitStreamEncoder::new(wheat_binschema::BitOrder::MsbFirst),
        }
    }
}

impl EntryScratch {
    fn encode(
        &mut self,
        entry: &ContentGenerationWireEntry,
    ) -> Result<&[u8], ContentGenerationWireError> {
        let wire = &mut self.entry;
        wire.id.digest.bytes.copy_from_slice(entry.id.as_bytes());
        wire.envelope_digest
            .bytes
            .copy_from_slice(&entry.envelope_digest);
        wire.pack_offset = entry.pack_offset;
        wire.physical_length = entry.physical_length;
        wire.logical_length = entry.logical_length;
        wire.delta_depth = entry.delta_depth;
        match entry.delta_base {
            Some(base) => {
                let mut id = wire
                    .delta_base
                    .take()
                    .or_else(|| self.spare_base.take())
                    .expect("the scratch holds one delta base");
                id.digest.bytes.copy_from_slice(base.as_bytes());
                wire.delta_base = Some(id);
            }
            None => {
                if let Some(id) = wire.delta_base.take() {
                    self.spare_base = Some(id);
                }
            }
        }
        self.encoder.clear();
        wire.encode_into(&mut self.encoder)
            .map_err(|error| ContentGenerationWireError::Decode(format!("{error:?}")))?;
        Ok(self.encoder.buffer())
    }
}

fn shard_ref_wire(shard: &ContentGenerationWireShardRef) -> wire::ContentGenerationShardRefV1 {
    wire::ContentGenerationShardRefV1 {
        first_id: content_id_wire(shard.first_id),
        entry_count: shard.entry_count,
        pack_length: shard.pack_length,
        manifest: digest_wire(shard.manifest),
    }
}

fn shard_ref_domain(
    shard: wire::ContentGenerationShardRefV1,
) -> Result<ContentGenerationWireShardRef, ContentGenerationWireError> {
    Ok(ContentGenerationWireShardRef {
        first_id: content_id_domain(shard.first_id)?,
        entry_count: shard.entry_count,
        pack_length: shard.pack_length,
        manifest: digest_domain(shard.manifest)?,
    })
}

fn entry_wire(entry: &ContentGenerationWireEntry) -> wire::ContentGenerationEntryV1 {
    wire::ContentGenerationEntryV1 {
        id: content_id_wire(entry.id),
        envelope_digest: digest_wire(entry.envelope_digest),
        pack_offset: entry.pack_offset,
        physical_length: entry.physical_length,
        logical_length: entry.logical_length,
        delta_base: entry.delta_base.map(content_id_wire),
        delta_depth: entry.delta_depth,
    }
}

fn entry_domain(
    entry: wire::ContentGenerationEntryV1,
) -> Result<ContentGenerationWireEntry, ContentGenerationWireError> {
    Ok(ContentGenerationWireEntry {
        id: content_id_domain(entry.id)?,
        envelope_digest: digest_domain(entry.envelope_digest)?,
        pack_offset: entry.pack_offset,
        physical_length: entry.physical_length,
        logical_length: entry.logical_length,
        delta_base: entry.delta_base.map(content_id_domain).transpose()?,
        delta_depth: entry.delta_depth,
    })
}

fn content_id_wire(id: ContentId) -> wire::ContentId {
    wire::ContentId {
        algorithm: 1,
        digest: digest_wire(*id.as_bytes()),
    }
}

fn content_id_domain(id: wire::ContentId) -> Result<ContentId, ContentGenerationWireError> {
    if id.algorithm != 1 {
        return Err(ContentGenerationWireError::InvalidContentIdAlgorithm);
    }
    Ok(ContentId::from_digest(digest_domain(id.digest)?))
}

fn digest_wire(bytes: [u8; 32]) -> wire::Digest256 {
    wire::Digest256 {
        bytes: bytes.to_vec(),
    }
}

fn digest_domain(digest: wire::Digest256) -> Result<[u8; 32], ContentGenerationWireError> {
    digest
        .bytes
        .try_into()
        .map_err(|_| ContentGenerationWireError::InvalidDigest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: u32 = CONTENT_GENERATION_SHARD_ENTRIES as u32;

    #[test]
    fn shard_layout_must_be_full_shards_in_id_order() {
        let ok = |entries: u64, counts: &[u32]| {
            check_shard_layout(entries, counts.iter().copied()).is_ok()
        };
        assert!(ok(0, &[]));
        assert!(ok(1, &[1]));
        assert!(ok(u64::from(FULL), &[FULL]));
        assert!(ok(u64::from(FULL) + 1, &[FULL, 1]));
        assert!(!ok(0, &[0]), "an empty generation has no shards");
        assert!(!ok(2, &[1, 1]), "only the last shard may be partial");
        assert!(!ok(u64::from(FULL) + 1, &[1, FULL]));
        assert!(!ok(u64::from(FULL), &[FULL, 0]));
        assert!(!ok(3, &[2]));
    }

    fn entry(n: u8, delta_base: Option<u8>) -> ContentGenerationWireEntry {
        ContentGenerationWireEntry {
            id: ContentId::from_digest([n; 32]),
            envelope_digest: [n.wrapping_add(1); 32],
            pack_offset: u64::from(n) * 10,
            physical_length: u64::from(n) + 1,
            logical_length: u64::from(n) + 2,
            delta_base: delta_base.map(|base| ContentId::from_digest([base; 32])),
            delta_depth: u8::from(delta_base.is_some()),
        }
    }

    #[test]
    fn fingerprint_hashes_each_entrys_own_encoding() {
        // Alternating delta bases exercise the scratch's spare base buffer.
        let entries = [
            entry(1, Some(9)),
            entry(2, None),
            entry(3, Some(8)),
            entry(4, Some(7)),
            entry(5, None),
        ];
        let mut expected = blake3::Hasher::new();
        expected.update(b"wheat-content-generation-v1\0");
        for entry in &entries {
            expected.update(&entry_wire(entry).encode().unwrap());
        }
        assert_eq!(
            content_generation_fingerprint(entries.iter().cloned()).unwrap(),
            *expected.finalize().as_bytes()
        );
        let mut moved = entries.clone();
        moved[4].pack_offset += 1;
        assert_ne!(
            content_generation_fingerprint(moved).unwrap(),
            content_generation_fingerprint(entries).unwrap()
        );
    }

    #[test]
    fn index_digest_covers_every_field() {
        let shard = ContentGenerationWireShardRef {
            first_id: ContentId::from_digest([1; 32]),
            entry_count: 1,
            pack_length: 10,
            manifest: [2; 32],
        };
        let digest = |generation, fingerprint, entries, shard: ContentGenerationWireShardRef| {
            content_generation_index_digest(generation, fingerprint, entries, [shard].into_iter())
                .unwrap()
        };
        let base = digest(1, [3; 32], 1, shard);
        assert_ne!(base, digest(2, [3; 32], 1, shard));
        assert_ne!(base, digest(1, [4; 32], 1, shard));
        assert_ne!(base, digest(1, [3; 32], 2, shard));
        for changed in [
            ContentGenerationWireShardRef {
                first_id: ContentId::from_digest([5; 32]),
                ..shard
            },
            ContentGenerationWireShardRef {
                entry_count: 2,
                ..shard
            },
            ContentGenerationWireShardRef {
                pack_length: 11,
                ..shard
            },
            ContentGenerationWireShardRef {
                manifest: [6; 32],
                ..shard
            },
        ] {
            assert_ne!(base, digest(1, [3; 32], 1, changed));
        }
        assert_ne!(
            base,
            content_generation_index_digest(1, [3; 32], 1, std::iter::empty()).unwrap()
        );
    }
}
