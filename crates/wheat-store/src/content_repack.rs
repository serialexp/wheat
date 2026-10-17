//! Content-generation repack planning and publication seams.
//!
//! Planning keeps only IDs and fixed-size physical metadata in memory. Logical
//! bodies and encoded envelopes are handled one at a time; envelopes are spilled
//! to an unlinked temporary file and streamed to the publication target.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::ContentId;

use crate::content_generation::{
    CONTENT_GENERATION_SHARD_ENTRIES, ContentGenerationEntry, ContentGenerationManifest,
    fingerprint_entries,
};
use crate::{ContentStore, ContentStoreError, codec, delta};

const MAX_DELTA_DEPTH: u8 = crate::content_store::MAX_DELTA_CHAIN_DEPTH as u8;

/// Hard limits for one repack planning pass.
#[derive(Clone, Copy, Debug)]
pub struct ContentRepackLimits {
    pub max_content_ids: usize,
    pub max_history_pairs: usize,
    pub max_logical_bytes: u64,
    /// Bounds each logical read and therefore the planner's working set. Delta
    /// planning has at most two logical records and two encoded candidates live.
    pub max_record_logical_bytes: usize,
}

impl Default for ContentRepackLimits {
    fn default() -> Self {
        Self {
            max_content_ids: 250_000,
            max_history_pairs: 4_000_000,
            // This is a work/capacity bound, not a memory budget: bodies spill.
            max_logical_bytes: 1 << 40,
            max_record_logical_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Exact authenticated inventory and retained content history.
///
/// Every inventory ID is retained even when it occurs in no pair or tip.
#[derive(Clone, Debug)]
pub struct ContentRepackInput {
    pub content_ids: Vec<ContentId>,
    /// Content of every file in a tip view (a view without children). Tips
    /// are always stored whole.
    pub tips: Vec<ContentId>,
    /// File content before and after single-parent view-lineage edges.
    pub history: Vec<ContentHistoryPair>,
}

/// One file's content in a parent view (`older`) and at the same path in its
/// only child view (`newer`). The newer content is the older's proposed
/// reverse-delta base.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentHistoryPair {
    pub older: ContentId,
    pub newer: ContentId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationQualifiedEnvelope {
    pub generation: u64,
    pub id: ContentId,
    pub bytes: Vec<u8>,
}

/// Read-only deterministic content plan: the sealed manifest plus the order in
/// which to produce its envelopes. Replacement bytes are generated after
/// capacity admission by [`prepare_content_repack`].
#[derive(Clone, Debug)]
pub struct ContentRepackAnalysis {
    pub manifest: ContentGenerationManifest,
    /// Manifest entry indexes in production order: grouped by shard, and
    /// within a shard every delta base before the deltas on it.
    production: Vec<u32>,
    limits: ContentRepackLimits,
    estimated_bytes: u64,
}

impl ContentRepackAnalysis {
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }
}

/// A complete generation whose potentially large envelope bodies live in scratch.
/// Dropping the value removes the scratch file.
#[derive(Debug)]
pub struct PreparedContentGeneration {
    pub manifest: ContentGenerationManifest,
    scratch: NamedTempFile,
    /// Manifest entry indexes in scratch order. Envelopes are contiguous in
    /// scratch, so each one's offset is the sum of the lengths before it.
    production: Vec<u32>,
    scratch_bytes: u64,
}

impl PreparedContentGeneration {
    /// Stream envelopes in generation order of production. Only one envelope body
    /// is resident at a time; callers need not (and must not) collect the iterator.
    pub fn envelopes(&self) -> Result<ContentEnvelopeIter<'_>, ContentRepackError> {
        Ok(ContentEnvelopeIter {
            generation: self.manifest.generation,
            entries: &self.manifest.entries,
            production: self.production.iter(),
            file: BufReader::with_capacity(64 * 1024, self.scratch.reopen()?),
        })
    }

    #[must_use]
    pub fn scratch_bytes(&self) -> u64 {
        self.scratch_bytes
    }
}

pub struct ContentEnvelopeIter<'a> {
    generation: u64,
    entries: &'a [ContentGenerationEntry],
    production: std::slice::Iter<'a, u32>,
    file: BufReader<File>,
}

impl Iterator for ContentEnvelopeIter<'_> {
    type Item = Result<GenerationQualifiedEnvelope, ContentRepackError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = &self.entries[*self.production.next()? as usize];
        Some((|| {
            let length = usize::try_from(entry.physical_length)
                .map_err(|_| ContentRepackError::ArithmeticOverflow("scratch envelope length"))?;
            let mut bytes = vec![0; length];
            self.file.read_exact(&mut bytes)?;
            Ok(GenerationQualifiedEnvelope {
                generation: self.generation,
                id: entry.id,
                bytes,
            })
        })())
    }
}

#[derive(Debug)]
pub enum ContentRepackPlan {
    NoOp {
        generation: u64,
        fingerprint: [u8; 32],
    },
    Replace(ContentRepackAnalysis),
}

#[derive(Debug, Error)]
pub enum ContentRepackError {
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("content repack {kind} limit exceeded ({actual} > {limit})")]
    Limit {
        kind: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("content repack arithmetic overflow while computing {0}")]
    ArithmeticOverflow(&'static str),
    #[error("content history references content {0} outside the authenticated inventory")]
    UnknownHistoryContent(ContentId),
    #[error("content generation overflow")]
    GenerationOverflow,
    #[error("invalid content generation manifest: {0}")]
    InvalidManifest(String),
    #[error("content generation integration failed: {0}")]
    Integration(String),
}

/// Atomic/recoverable integration boundary for a physical content catalog.
pub trait ContentGenerationTarget {
    /// Durably consume `prepared.envelopes()` before returning. Implementations
    /// must stream it rather than retaining all envelope bodies.
    fn stage(&self, prepared: &PreparedContentGeneration) -> Result<(), String>;
    fn publish(
        &self,
        expected_generation: Option<u64>,
        manifest: &ContentGenerationManifest,
    ) -> Result<(), String>;
    fn recover(&self) -> Result<(), String>;
}

/// Validate the scratch generation, stage it, then atomically publish it.
pub fn publish_content_repack(
    target: &impl ContentGenerationTarget,
    expected_generation: Option<u64>,
    prepared: &PreparedContentGeneration,
) -> Result<(), ContentRepackError> {
    prepared.manifest.verify()?;
    let mut seen = 0usize;
    for envelope in prepared.envelopes()? {
        let envelope = envelope?;
        let entry = prepared
            .manifest
            .entries
            .binary_search_by_key(&envelope.id, |entry| entry.id)
            .ok()
            .map(|index| &prepared.manifest.entries[index])
            .ok_or_else(|| ContentRepackError::InvalidManifest("scratch has unknown ID".into()))?;
        if envelope.generation != prepared.manifest.generation
            || u64::try_from(envelope.bytes.len()).ok() != Some(entry.physical_length)
            || *blake3::hash(&envelope.bytes).as_bytes() != entry.envelope_digest
        {
            return Err(ContentRepackError::InvalidManifest(
                "generation envelopes differ from manifest".into(),
            ));
        }
        seen = seen
            .checked_add(1)
            .ok_or(ContentRepackError::ArithmeticOverflow(
                "verified envelope count",
            ))?;
    }
    if seen != prepared.manifest.entries.len() {
        return Err(ContentRepackError::InvalidManifest(
            "generation envelope count differs from manifest".into(),
        ));
    }
    target
        .stage(prepared)
        .map_err(ContentRepackError::Integration)?;
    target
        .publish(expected_generation, &prepared.manifest)
        .map_err(ContentRepackError::Integration)
}

/// Base-table mark: the ID has no proposed base (or is a tip).
const NO_BASE: u32 = u32::MAX;
/// Base-table mark: two different bases were proposed, so the ID stays whole.
const CONFLICT: u32 = u32::MAX - 1;
/// Inventory positions are `u32`; the two largest values are the marks above.
const MAX_INDEXED_CONTENT_IDS: usize = CONFLICT as usize;

/// Where planning stands for one inventory position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Visit {
    Pending,
    OnPath,
    Chosen,
}

/// Build a deterministic reverse-delta generation with disk-backed staging.
///
/// Memory is a few flat per-ID tables indexed by inventory position (the
/// manifest entries themselves, a `u32` base, a one-byte visit mark and a
/// `u32` production slot), never maps keyed by `ContentId`.
pub fn plan_content_repack(
    source: &ContentStore,
    current: Option<&ContentGenerationManifest>,
    input: ContentRepackInput,
    limits: ContentRepackLimits,
) -> Result<ContentRepackPlan, ContentRepackError> {
    if let Some(current) = current {
        current.verify()?;
    }
    let ContentRepackInput {
        content_ids: mut inventory,
        tips,
        history,
    } = input;
    inventory.sort_unstable();
    inventory.dedup();
    check_limit("content IDs", inventory.len(), limits.max_content_ids)?;
    check_limit(
        "indexed content IDs",
        inventory.len(),
        MAX_INDEXED_CONTENT_IDS,
    )?;
    check_limit("history pairs", history.len(), limits.max_history_pairs)?;
    let position = |id: &ContentId| {
        inventory
            .binary_search(id)
            .map(|index| index as u32)
            .map_err(|_| ContentRepackError::UnknownHistoryContent(*id))
    };

    // Reverse deltas: the older content is the target, the newer its base.
    let mut bases = vec![NO_BASE; inventory.len()];
    for pair in &history {
        let older = position(&pair.older)?;
        let newer = position(&pair.newer)?;
        let proposed = if older == newer { CONFLICT } else { newer };
        let slot = &mut bases[older as usize];
        if *slot == NO_BASE {
            *slot = proposed;
        } else if *slot != proposed {
            *slot = CONFLICT;
        }
    }
    drop(history);
    for tip in &tips {
        bases[position(tip)? as usize] = NO_BASE;
    }
    drop(tips);

    // Entries are filled in place, in ID order; `inventory[i]` is `entries[i].id`.
    let mut entries = inventory
        .iter()
        .map(|id| ContentGenerationEntry {
            id: *id,
            envelope_digest: [0; 32],
            pack_offset: 0,
            physical_length: 0,
            logical_length: 0,
            delta_base: None,
            delta_depth: 0,
        })
        .collect::<Vec<_>>();
    drop(inventory);
    let mut visit = vec![Visit::Pending; entries.len()];
    let mut production = Vec::<u32>::with_capacity(entries.len());
    let mut estimated_bytes = 0u64;
    let mut logical_bytes = 0u64;
    let mut path = Vec::<usize>::new();

    for start in 0..entries.len() {
        if visit[start] == Visit::Chosen {
            continue;
        }
        path.clear();
        let mut cursor = start;
        let cycle = loop {
            if visit[cursor] == Visit::Chosen || bases[cursor] >= CONFLICT {
                break false;
            }
            if visit[cursor] == Visit::OnPath {
                break true;
            }
            visit[cursor] = Visit::OnPath;
            path.push(cursor);
            cursor = bases[cursor] as usize;
        };
        if visit[cursor] != Visit::Chosen {
            path.push(cursor);
        }
        for &index in path.iter().rev() {
            if visit[index] == Visit::Chosen {
                continue;
            }
            let base = match bases[index] {
                base if !cycle && base < CONFLICT && visit[base as usize] == Visit::Chosen => {
                    Some(base as usize)
                }
                _ => None,
            };
            let id = entries[index].id;
            let target_bytes = source.get_bounded(id, limits.max_record_logical_bytes)?;
            let target_length = u64::try_from(target_bytes.len())
                .map_err(|_| ContentRepackError::ArithmeticOverflow("record logical length"))?;
            logical_bytes = logical_bytes
                .checked_add(target_length)
                .ok_or(ContentRepackError::ArithmeticOverflow("logical byte count"))?;
            if logical_bytes > limits.max_logical_bytes {
                return Err(ContentRepackError::Limit {
                    kind: "logical bytes",
                    actual: logical_bytes,
                    limit: limits.max_logical_bytes,
                });
            }
            let full = encode_full(id, &target_bytes)?;
            let (bytes, selected_base, depth) = match base {
                Some(base) if entries[base].delta_depth < MAX_DELTA_DEPTH => {
                    let (base_id, base_depth) = (entries[base].id, entries[base].delta_depth);
                    let base_bytes =
                        source.get_bounded(base_id, limits.max_record_logical_bytes)?;
                    let candidate = delta::compute_delta(&base_bytes, &target_bytes)
                        .and_then(|patch| {
                            codec::encode_delta_envelope(target_bytes.len(), base_id, &patch).ok()
                        })
                        .filter(|candidate| candidate.len() < full.len());
                    candidate.map_or((full, None, 0), |bytes| {
                        (bytes, Some(base_id), base_depth + 1)
                    })
                }
                _ => (full, None, 0),
            };
            let physical_length = u64::try_from(bytes.len())
                .map_err(|_| ContentRepackError::ArithmeticOverflow("envelope length"))?;
            estimated_bytes = estimated_bytes.checked_add(physical_length).ok_or(
                ContentRepackError::ArithmeticOverflow("replacement byte estimate"),
            )?;
            let entry = &mut entries[index];
            entry.envelope_digest = *blake3::hash(&bytes).as_bytes();
            entry.physical_length = physical_length;
            entry.logical_length = target_length;
            entry.delta_base = selected_base;
            entry.delta_depth = depth;
            visit[index] = Visit::Chosen;
            production.push(index as u32);
        }
    }
    drop((bases, visit, path));

    // Each shard has its own pack: group production by shard (keeping the
    // base-first order within it) and lay each pack out from offset zero.
    production.sort_by_key(|index| *index as usize / CONTENT_GENERATION_SHARD_ENTRIES);
    let mut shard = usize::MAX;
    let mut next_offset = 0u64;
    for &index in &production {
        let index = index as usize;
        if index / CONTENT_GENERATION_SHARD_ENTRIES != shard {
            shard = index / CONTENT_GENERATION_SHARD_ENTRIES;
            next_offset = 0;
        }
        entries[index].pack_offset = next_offset;
        next_offset = next_offset
            .checked_add(entries[index].physical_length)
            .ok_or(ContentRepackError::ArithmeticOverflow(
                "content pack offset",
            ))?;
    }
    let content_fingerprint = fingerprint_entries(&entries)?;
    if let Some(current) = current
        && current.content_fingerprint == content_fingerprint
    {
        return Ok(ContentRepackPlan::NoOp {
            generation: current.generation,
            fingerprint: content_fingerprint,
        });
    }
    let generation = current.map_or(Ok(0), |manifest| {
        manifest
            .generation
            .checked_add(1)
            .ok_or(ContentRepackError::GenerationOverflow)
    })?;
    Ok(ContentRepackPlan::Replace(ContentRepackAnalysis {
        manifest: ContentGenerationManifest::new(generation, entries)?,
        production,
        limits,
        estimated_bytes,
    }))
}

/// Execute a previously analysed content plan into disk-backed scratch.
///
/// Every envelope is re-encoded from the source and must match its planned
/// length and digest exactly, and together they must match the planned byte
/// total. That binds execution to the plan without planning a second time.
pub fn prepare_content_repack(
    source: &ContentStore,
    analysis: ContentRepackAnalysis,
) -> Result<PreparedContentGeneration, ContentRepackError> {
    let ContentRepackAnalysis {
        manifest,
        production,
        limits,
        estimated_bytes,
    } = analysis;
    if production.len() != manifest.entries.len() {
        return Err(ContentRepackError::InvalidManifest(
            "production order does not cover the manifest".into(),
        ));
    }
    let mut scratch = NamedTempFile::new()?;
    let mut writer = BufWriter::new(scratch.as_file_mut());
    let mut scratch_bytes = 0u64;
    for &index in &production {
        let entry = manifest.entries.get(index as usize).ok_or_else(|| {
            ContentRepackError::InvalidManifest("production index is outside the manifest".into())
        })?;
        let bytes = encode_planned_envelope(source, entry, limits)?;
        if u64::try_from(bytes.len()).ok() != Some(entry.physical_length)
            || blake3::hash(&bytes).as_bytes() != &entry.envelope_digest
        {
            return Err(ContentRepackError::InvalidManifest(
                "executed envelope differs from planned metadata".into(),
            ));
        }
        scratch_bytes = scratch_bytes
            .checked_add(entry.physical_length)
            .ok_or(ContentRepackError::ArithmeticOverflow("scratch byte count"))?;
        writer.write_all(&bytes)?;
    }
    writer.flush()?;
    drop(writer);
    if scratch_bytes != estimated_bytes {
        return Err(ContentRepackError::InvalidManifest(
            "executed byte total differs from the plan".into(),
        ));
    }
    Ok(PreparedContentGeneration {
        manifest,
        scratch,
        production,
        scratch_bytes,
    })
}

fn encode_planned_envelope(
    source: &ContentStore,
    entry: &ContentGenerationEntry,
    limits: ContentRepackLimits,
) -> Result<Vec<u8>, ContentRepackError> {
    let id = entry.id;
    let target = source.get_bounded(id, limits.max_record_logical_bytes)?;
    match entry.delta_base {
        Some(base) => {
            let base_bytes = source.get_bounded(base, limits.max_record_logical_bytes)?;
            let patch = delta::compute_delta(&base_bytes, &target).ok_or_else(|| {
                ContentRepackError::InvalidManifest("planned delta is no longer computable".into())
            })?;
            codec::encode_delta_envelope(target.len(), base, &patch).map_err(|error| {
                ContentStoreError::InvalidFormat {
                    id,
                    reason: error.to_string(),
                }
                .into()
            })
        }
        None => encode_full(id, &target),
    }
}

fn encode_full(id: ContentId, bytes: &[u8]) -> Result<Vec<u8>, ContentRepackError> {
    codec::encode_envelope(bytes).map_err(|error| {
        ContentStoreError::InvalidFormat {
            id,
            reason: error.to_string(),
        }
        .into()
    })
}

fn check_limit(kind: &'static str, actual: usize, limit: usize) -> Result<(), ContentRepackError> {
    if actual > limit {
        return Err(ContentRepackError::Limit {
            kind,
            actual: u64::try_from(actual)
                .map_err(|_| ContentRepackError::ArithmeticOverflow("limit actual"))?,
            limit: u64::try_from(limit)
                .map_err(|_| ContentRepackError::ArithmeticOverflow("limit bound"))?,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_id_planner_state_is_small_and_fixed_size() {
        // Beyond the manifest entry every ID costs a u32 base, a one-byte visit
        // mark and a u32 production slot. Payload bytes live only in scratch.
        assert_eq!(std::mem::size_of::<Visit>(), 1);
        assert!(std::mem::size_of::<ContentGenerationEntry>() <= 128);
        let per_id = std::mem::size_of::<ContentGenerationEntry>()
            + 2 * std::mem::size_of::<u32>()
            + std::mem::size_of::<Visit>();
        assert!(per_id <= 144, "{per_id} bytes per content ID");
    }
}
