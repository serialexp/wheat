//! Content-generation repack planning and publication seams.
//!
//! Planning keeps only IDs and fixed-size physical metadata in memory. Logical
//! bodies and encoded envelopes are handled one at a time; envelopes are spilled
//! to an unlinked temporary file and streamed to the publication target.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{ContentId, Repository};

use crate::{ContentStore, ContentStoreError, codec, delta};

const MAX_DELTA_DEPTH: u8 = crate::content_store::MAX_DELTA_CHAIN_DEPTH as u8;

/// Hard limits for one repack planning pass.
#[derive(Clone, Copy, Debug)]
pub struct ContentRepackLimits {
    pub max_content_ids: usize,
    pub max_lineages: usize,
    pub max_lineage_entries: usize,
    pub max_logical_bytes: u64,
    /// Bounds each logical read and therefore the planner's working set. Delta
    /// planning has at most two logical records and two encoded candidates live.
    pub max_record_logical_bytes: usize,
}

impl Default for ContentRepackLimits {
    fn default() -> Self {
        Self {
            max_content_ids: 250_000,
            max_lineages: 250_000,
            max_lineage_entries: 4_000_000,
            // This is a work/capacity bound, not a memory budget: bodies spill.
            max_logical_bytes: 1 << 40,
            max_record_logical_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Exact authenticated inventory and retained content lineages.
///
/// Lineages are ordered newest (the retained tip) to oldest. Every inventory ID
/// is retained even when it occurs in no lineage.
#[derive(Clone, Debug)]
pub struct ContentRepackInput {
    pub content_ids: Vec<ContentId>,
    pub lineages_newest_first: Vec<Vec<ContentId>>,
}

/// Derive deterministic per-path content histories from authenticated retained
/// views. A merge with conflicting parents terminates both histories rather than
/// choosing a representation based on traversal order.
pub(crate) fn retained_lineages(repository: &Repository) -> Vec<Vec<ContentId>> {
    let views = repository
        .retained_views()
        .map(|view| (view.id, view))
        .collect::<BTreeMap<_, _>>();
    let mut parents = BTreeMap::<_, Vec<_>>::new();
    let mut children = BTreeMap::<_, Vec<_>>::new();
    for (parent, child) in repository.view_lineage() {
        parents.entry(child).or_default().push(parent);
        children.entry(parent).or_default().push(child);
    }
    for values in parents.values_mut() {
        values.sort_unstable();
    }
    for values in children.values_mut() {
        values.sort_unstable();
    }

    let mut result = Vec::new();
    for (tip_id, tip) in &views {
        if children
            .get(tip_id)
            .is_some_and(|values| !values.is_empty())
        {
            continue;
        }
        for path in tip.tree.keys() {
            let mut lineage = Vec::new();
            let mut cursor = *tip_id;
            while let Some(view) = views.get(&cursor) {
                let Some(content) = view.tree.get(path).map(|value| value.content) else {
                    break;
                };
                if lineage.last() != Some(&content) {
                    lineage.push(content);
                }
                let Some(parent) = parents.get(&cursor).filter(|values| values.len() == 1) else {
                    break;
                };
                cursor = parent[0];
            }
            if !lineage.is_empty() {
                result.push(lineage);
            }
        }
    }
    result.sort();
    result.dedup();
    result
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentGenerationEntry {
    pub id: ContentId,
    pub envelope_digest: [u8; 32],
    /// Offset in the generation's single immutable `content.pack` file.
    pub pack_offset: u64,
    pub physical_length: u64,
    pub logical_length: u64,
    pub delta_base: Option<ContentId>,
    pub delta_depth: u8,
}

/// Authenticated description of one complete physical content generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentGenerationManifest {
    pub generation: u64,
    pub entries: Vec<ContentGenerationEntry>,
    /// Generation-independent representation fingerprint used for no-op detection.
    pub content_fingerprint: [u8; 32],
    /// Digest over the generation number, fingerprint, and all entries.
    pub manifest_digest: [u8; 32],
}

impl ContentGenerationManifest {
    /// Authenticate the manifest and reject non-canonical entry ordering.
    pub fn verify(&self) -> Result<(), ContentRepackError> {
        if self.entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(ContentRepackError::InvalidManifest(
                "entries are not strictly ordered".into(),
            ));
        }
        let content_fingerprint = fingerprint_entries(&self.entries, None);
        if content_fingerprint != self.content_fingerprint {
            return Err(ContentRepackError::InvalidManifest(
                "content fingerprint differs".into(),
            ));
        }
        let digest = fingerprint_entries(
            &self.entries,
            Some((self.generation, self.content_fingerprint)),
        );
        if digest != self.manifest_digest {
            return Err(ContentRepackError::InvalidManifest(
                "manifest digest differs".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn encoded(&self) -> Result<Vec<u8>, ContentRepackError> {
        serde_json::to_vec(self)
            .map_err(|error| ContentRepackError::InvalidManifest(error.to_string()))
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, ContentRepackError> {
        let manifest: Self = serde_json::from_slice(encoded)
            .map_err(|error| ContentRepackError::InvalidManifest(error.to_string()))?;
        manifest.verify()?;
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationQualifiedEnvelope {
    pub generation: u64,
    pub id: ContentId,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct ScratchEnvelope {
    entry_index: usize,
    offset: u64,
    length: u64,
}

/// Read-only deterministic content plan. It contains fixed-size authenticated
/// metadata and source IDs only; replacement bytes are generated after capacity
/// admission by [`prepare_content_repack`].
#[derive(Clone, Debug)]
pub struct ContentRepackAnalysis {
    pub manifest: ContentGenerationManifest,
    input: ContentRepackInput,
    production_ids: Vec<ContentId>,
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
    physical_order: Vec<ScratchEnvelope>,
    scratch_bytes: u64,
}

impl PreparedContentGeneration {
    /// Stream envelopes in generation order of production. Only one envelope body
    /// is resident at a time; callers need not (and must not) collect the iterator.
    pub fn envelopes(&self) -> Result<ContentEnvelopeIter<'_>, ContentRepackError> {
        Ok(ContentEnvelopeIter {
            generation: self.manifest.generation,
            entries: &self.manifest.entries,
            locations: self.physical_order.iter(),
            file: self.scratch.reopen()?,
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
    locations: std::slice::Iter<'a, ScratchEnvelope>,
    file: File,
}

impl Iterator for ContentEnvelopeIter<'_> {
    type Item = Result<GenerationQualifiedEnvelope, ContentRepackError>;

    fn next(&mut self) -> Option<Self::Item> {
        let location = self.locations.next()?;
        Some((|| {
            let length = usize::try_from(location.length)
                .map_err(|_| ContentRepackError::ArithmeticOverflow("scratch envelope length"))?;
            self.file.seek(SeekFrom::Start(location.offset))?;
            let mut bytes = vec![0; length];
            self.file.read_exact(&mut bytes)?;
            Ok(GenerationQualifiedEnvelope {
                generation: self.generation,
                id: self.entries[location.entry_index].id,
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
    #[doc(hidden)]
    ReplacePrepared(PreparedContentGeneration),
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
    #[error("lineage references content {0} outside the authenticated inventory")]
    UnknownLineageContent(ContentId),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BaseChoice {
    Unique(ContentId),
    Conflict,
}

#[derive(Clone, Copy, Debug)]
struct ChosenEnvelope {
    physical_length: u64,
    logical_length: u64,
    digest: [u8; 32],
    base: Option<ContentId>,
    depth: u8,
}

/// Build a deterministic reverse-delta generation with disk-backed staging.
pub fn plan_content_repack(
    source: &ContentStore,
    current: Option<&ContentGenerationManifest>,
    input: ContentRepackInput,
    limits: ContentRepackLimits,
) -> Result<ContentRepackPlan, ContentRepackError> {
    if let Some(current) = current {
        current.verify()?;
    }
    let inventory = input.content_ids.into_iter().collect::<BTreeSet<_>>();
    check_limit("content IDs", inventory.len(), limits.max_content_ids)?;
    check_limit(
        "lineages",
        input.lineages_newest_first.len(),
        limits.max_lineages,
    )?;
    let lineage_entries = input
        .lineages_newest_first
        .iter()
        .try_fold(0usize, |sum, lineage| sum.checked_add(lineage.len()))
        .ok_or(ContentRepackError::ArithmeticOverflow(
            "lineage entry count",
        ))?;
    check_limit(
        "lineage entries",
        lineage_entries,
        limits.max_lineage_entries,
    )?;

    let mut tips = BTreeSet::new();
    let mut bases = BTreeMap::<ContentId, BaseChoice>::new();
    for lineage in &input.lineages_newest_first {
        if let Some(tip) = lineage.first() {
            tips.insert(*tip);
        }
        for id in lineage {
            if !inventory.contains(id) {
                return Err(ContentRepackError::UnknownLineageContent(*id));
            }
        }
        for pair in lineage.windows(2) {
            let (base, target) = (pair[0], pair[1]);
            let proposed = if base == target {
                BaseChoice::Conflict
            } else {
                BaseChoice::Unique(base)
            };
            bases
                .entry(target)
                .and_modify(|choice| {
                    if *choice != proposed {
                        *choice = BaseChoice::Conflict;
                    }
                })
                .or_insert(proposed);
        }
    }
    for tip in &tips {
        bases.remove(tip);
    }

    let mut estimated_bytes = 0u64;
    let mut logical_bytes = 0u64;
    let mut chosen = BTreeMap::<ContentId, ChosenEnvelope>::new();
    let mut production_ids = Vec::with_capacity(inventory.len());
    let mut path = Vec::new();
    let mut on_path = HashSet::new();

    for start in &inventory {
        if chosen.contains_key(start) {
            continue;
        }
        path.clear();
        on_path.clear();
        let mut cursor = *start;
        let cycle = loop {
            if chosen.contains_key(&cursor)
                || !matches!(bases.get(&cursor), Some(BaseChoice::Unique(_)))
            {
                break false;
            }
            if !on_path.insert(cursor) {
                break true;
            }
            path.push(cursor);
            let Some(BaseChoice::Unique(base)) = bases.get(&cursor) else {
                unreachable!()
            };
            cursor = *base;
        };
        if !chosen.contains_key(&cursor) {
            path.push(cursor);
        }
        for id in path.iter().rev() {
            if chosen.contains_key(id) {
                continue;
            }
            let base = if cycle {
                None
            } else {
                bases.get(id).and_then(|choice| match choice {
                    BaseChoice::Unique(base) if chosen.contains_key(base) => Some(*base),
                    BaseChoice::Unique(_) | BaseChoice::Conflict => None,
                })
            };
            let target_bytes = source.get_bounded(*id, limits.max_record_logical_bytes)?;
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
            let full = encode_full(*id, &target_bytes)?;
            let (bytes, selected_base, depth) = if let Some(base_id) = base {
                let base_meta = chosen[&base_id];
                if base_meta.depth < MAX_DELTA_DEPTH {
                    let base_bytes =
                        source.get_bounded(base_id, limits.max_record_logical_bytes)?;
                    let candidate = delta::compute_delta(&base_bytes, &target_bytes)
                        .and_then(|patch| {
                            codec::encode_delta_envelope(target_bytes.len(), base_id, &patch).ok()
                        })
                        .filter(|candidate| candidate.len() < full.len());
                    candidate.map_or((full, None, 0), |bytes| {
                        (bytes, Some(base_id), base_meta.depth + 1)
                    })
                } else {
                    (full, None, 0)
                }
            } else {
                (full, None, 0)
            };
            let physical_length = u64::try_from(bytes.len())
                .map_err(|_| ContentRepackError::ArithmeticOverflow("envelope length"))?;
            estimated_bytes = estimated_bytes.checked_add(physical_length).ok_or(
                ContentRepackError::ArithmeticOverflow("replacement byte estimate"),
            )?;
            let digest = *blake3::hash(&bytes).as_bytes();
            chosen.insert(
                *id,
                ChosenEnvelope {
                    physical_length,
                    logical_length: target_length,
                    digest,
                    base: selected_base,
                    depth,
                },
            );
            production_ids.push(*id);
        }
    }

    let mut offsets = BTreeMap::new();
    let mut next_offset = 0u64;
    for id in &production_ids {
        offsets.insert(*id, next_offset);
        next_offset = next_offset.checked_add(chosen[id].physical_length).ok_or(
            ContentRepackError::ArithmeticOverflow("content pack offset"),
        )?;
    }
    let mut entries = Vec::with_capacity(chosen.len());
    for (id, envelope) in &chosen {
        entries.push(ContentGenerationEntry {
            id: *id,
            envelope_digest: envelope.digest,
            pack_offset: offsets[id],
            physical_length: envelope.physical_length,
            logical_length: envelope.logical_length,
            delta_base: envelope.base,
            delta_depth: envelope.depth,
        });
    }
    let content_fingerprint = fingerprint_entries(&entries, None);
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
    let manifest_digest = fingerprint_entries(&entries, Some((generation, content_fingerprint)));
    Ok(ContentRepackPlan::Replace(ContentRepackAnalysis {
        manifest: ContentGenerationManifest {
            generation,
            entries,
            content_fingerprint,
            manifest_digest,
        },
        input: ContentRepackInput {
            content_ids: inventory.into_iter().collect(),
            lineages_newest_first: input.lineages_newest_first,
        },
        production_ids,
        limits,
        estimated_bytes,
    }))
}

/// Execute a previously analysed content plan into disk-backed scratch. The
/// deterministic manifest is re-derived and compared before any result can be
/// staged, binding execution to the exact authority inspected during planning.
pub fn prepare_content_repack(
    source: &ContentStore,
    analysis: &ContentRepackAnalysis,
) -> Result<PreparedContentGeneration, ContentRepackError> {
    let replanned = plan_content_repack(source, None, analysis.input.clone(), analysis.limits)?;
    let ContentRepackPlan::Replace(replanned) = replanned else {
        return Err(ContentRepackError::InvalidManifest(
            "execution unexpectedly produced a no-op analysis".into(),
        ));
    };
    if replanned.manifest.content_fingerprint != analysis.manifest.content_fingerprint
        || replanned.manifest.entries != analysis.manifest.entries
        || replanned.production_ids != analysis.production_ids
        || replanned.estimated_bytes != analysis.estimated_bytes
    {
        return Err(ContentRepackError::InvalidManifest(
            "content source changed after planning".into(),
        ));
    }

    let mut scratch = NamedTempFile::new()?;
    let mut writer = BufWriter::new(scratch.as_file_mut());
    let entry_indexes = analysis
        .manifest
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id, index))
        .collect::<BTreeMap<_, _>>();
    let mut physical_order = Vec::with_capacity(analysis.production_ids.len());
    let mut scratch_bytes = 0u64;
    for id in &analysis.production_ids {
        let bytes = encode_planned_envelope(source, &analysis.manifest, *id, analysis.limits)?;
        let entry_index = *entry_indexes
            .get(id)
            .ok_or_else(|| ContentRepackError::InvalidManifest("production ID is absent".into()))?;
        let entry = &analysis.manifest.entries[entry_index];
        if u64::try_from(bytes.len()).ok() != Some(entry.physical_length)
            || blake3::hash(&bytes).as_bytes() != &entry.envelope_digest
        {
            return Err(ContentRepackError::InvalidManifest(
                "executed envelope differs from planned metadata".into(),
            ));
        }
        let offset = scratch_bytes;
        scratch_bytes = scratch_bytes
            .checked_add(entry.physical_length)
            .ok_or(ContentRepackError::ArithmeticOverflow("scratch byte count"))?;
        writer.write_all(&bytes)?;
        physical_order.push(ScratchEnvelope {
            entry_index,
            offset,
            length: entry.physical_length,
        });
    }
    writer.flush()?;
    drop(writer);
    Ok(PreparedContentGeneration {
        manifest: analysis.manifest.clone(),
        scratch,
        physical_order,
        scratch_bytes,
    })
}

fn encode_planned_envelope(
    source: &ContentStore,
    manifest: &ContentGenerationManifest,
    id: ContentId,
    limits: ContentRepackLimits,
) -> Result<Vec<u8>, ContentRepackError> {
    let entry = manifest
        .entries
        .binary_search_by_key(&id, |entry| entry.id)
        .ok()
        .map(|index| &manifest.entries[index])
        .ok_or_else(|| ContentRepackError::InvalidManifest("planned ID is absent".into()))?;
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

fn fingerprint_entries(
    entries: &[ContentGenerationEntry],
    generation: Option<(u64, [u8; 32])>,
) -> [u8; 32] {
    let mut hash = Hasher::new();
    hash.update(b"wheat-content-generation-v1\0");
    if let Some((generation, content_fingerprint)) = generation {
        hash.update(&generation.to_be_bytes());
        hash.update(&content_fingerprint);
    }
    let entry_count = u64::try_from(entries.len()).expect("usize must fit in u64");
    hash.update(&entry_count.to_be_bytes());
    for entry in entries {
        hash.update(entry.id.as_bytes());
        hash.update(&entry.envelope_digest);
        hash.update(&entry.pack_offset.to_be_bytes());
        hash.update(&entry.physical_length.to_be_bytes());
        hash.update(&entry.logical_length.to_be_bytes());
        match entry.delta_base {
            Some(base) => {
                hash.update(&[1]);
                hash.update(base.as_bytes());
            }
            None => {
                hash.update(&[0]);
            }
        }
        hash.update(&[entry.delta_depth]);
    }
    *hash.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_million_record_metadata_has_no_body_sized_fields() {
        // The planner's per-record state is fixed-size. At the production limit,
        // payload bytes remain exclusively in scratch instead of multiplying RSS.
        let records = ContentRepackLimits::default().max_content_ids;
        assert!(std::mem::size_of::<ChosenEnvelope>() <= 96);
        assert!(std::mem::size_of::<ScratchEnvelope>() <= 32);
        assert!(
            records
                .checked_mul(std::mem::size_of::<ChosenEnvelope>())
                .unwrap()
                <= 24 * 1024 * 1024
        );
    }
}
