//! `wheat store analyze` — storage breakdown for a wheat repository.
//!
//! Walks packed segments, loose objects, and loose content to produce a
//! table showing where bytes live and what kinds of data they hold.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use wheat_core::ContentId;
#[cfg(test)]
use wheat_core::{PhysicalCatalogSnapshot, encode_physical_catalog_snapshot};
use wheat_core::{
    PhysicalContentGeneration, SEGMENT_MAX_DELTA_DEPTH, decode_physical_catalog_snapshot,
    validate_canonical_object,
};
use wheat_store::packed_segment_reader::{self, read_loose_envelope};
#[cfg(test)]
use wheat_store::{ContentGenerationManifest, content_generation_shard_manifest_path};
use wheat_store::{ObjectKind, content_generation_shard_pack_path, read_content_generation};

mod semantic;

/// Metadata entries per delta-chain depth; index 0 counts whole records.
type DeltaDepthHistogram = [u64; SEGMENT_MAX_DELTA_DEPTH as usize + 1];

/// Human-readable name for a segment kind code.
fn kind_name(kind: u16) -> &'static str {
    ObjectKind::from_code(kind)
        .map(|k| match k {
            ObjectKind::Tree => "Tree",
            ObjectKind::Change => "Change",
            ObjectKind::Revision => "Revision",
            ObjectKind::Application => "Application",
            ObjectKind::Conflict => "Conflict",
            ObjectKind::View => "View",
            ObjectKind::ContentMapNode => "ContentMapNode",
            ObjectKind::RepositoryState => "RepositoryState",
            ObjectKind::RevisionMapNode => "RevisionMapNode",
            ObjectKind::ApplicationMapNode => "ApplicationMapNode",
            ObjectKind::ConflictSetNode => "ConflictSetNode",
            ObjectKind::ContentSetNode => "ContentSetNode",
            ObjectKind::RetainedSetNode => "RetainedSetNode",
            ObjectKind::ViewLabelManifest => "ViewLabelManifest",
            ObjectKind::RepositoryEnvelope => "Envelope",
            ObjectKind::Edit => "Edit",
            ObjectKind::ChangeMapNode => "ChangeMapNode",
            ObjectKind::ViewLineageMapNode => "ViewLineageMapNode",
        })
        .unwrap_or("Unknown")
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn is_git_equivalent_kind(kind: u16) -> bool {
    matches!(
        ObjectKind::from_code(kind),
        Some(ObjectKind::Tree | ObjectKind::Revision | ObjectKind::ViewLabelManifest)
    )
}

fn codec_name(codec: u8) -> &'static str {
    match codec {
        0 => "Raw",
        1 => "Zstd",
        2 => "Delta",
        3 => "DeltaZstd",
        4 => "ZstdBlock",
        _ => "?",
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Apparent length and filesystem-allocated length for a regular file.
///
/// `allocated_bytes` is based on the filesystem's 512-byte block count, so it
/// captures sparse files and allocation rounding rather than merely repeating
/// the apparent length.
#[derive(Clone, Copy, Default)]
struct FileBytes {
    apparent_bytes: u64,
    allocated_bytes: u64,
}

impl FileBytes {
    fn add_metadata(&mut self, metadata: &fs::Metadata) {
        self.apparent_bytes += metadata.len();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            self.allocated_bytes += metadata.blocks().saturating_mul(512);
        }
        #[cfg(not(unix))]
        {
            self.allocated_bytes += metadata.len();
        }
    }
}

#[derive(Clone, Copy)]
struct SelectedContentGeneration {
    generation: u64,
    manifest: [u8; 32],
}

/// Reads the catalog snapshot directly without opening or recovering the
/// store. A missing or malformed authority, or a snapshot that has not
/// selected a content generation yet, has no selected generation.
fn selected_content_generation(storage_root: &Path) -> Option<SelectedContentGeneration> {
    const HEAD_MAGIC: &[u8] = b"WHEATPACK";
    const HEAD_LEN: usize = HEAD_MAGIC.len() + 1 + 8 + 32;

    let head = fs::read(storage_root.join("packed/catalog.head")).ok()?;
    if head.len() != HEAD_LEN || !head.starts_with(HEAD_MAGIC) || head[HEAD_MAGIC.len()] != 1 {
        return None;
    }
    let generation = u64::from_be_bytes(head[10..18].try_into().ok()?);
    let digest: [u8; 32] = head[18..50].try_into().ok()?;
    let catalog = fs::read(
        storage_root
            .join("packed/catalog")
            .join(format!("{generation}-{}.catalog", hex_encode(&digest))),
    )
    .ok()?;
    if blake3::hash(&catalog).as_bytes() != &digest {
        return None;
    }
    let snapshot = decode_physical_catalog_snapshot(&catalog).ok()?;
    (snapshot.generation == generation && snapshot.content.manifest != [0; 32]).then_some(
        SelectedContentGeneration {
            generation: snapshot.content.generation,
            manifest: snapshot.content.manifest,
        },
    )
}

/// Per-kind accumulator for segment entries.
#[derive(Default)]
struct KindStats {
    count: u64,
    /// Entries stored as in-segment deltas.
    delta_count: u64,
    logical_bytes: u64,
    /// Stored bytes with block dedup — for block entries we track unique blocks.
    stored_bytes: u64,
}

/// Per-codec accumulator.
#[derive(Default)]
struct CodecStats {
    count: u64,
    stored_bytes: u64,
}

/// Per-kind accumulator for loose objects (identified by reading their canonical kind).
#[derive(Default)]
struct LooseKindStats {
    count: u64,
    disk_bytes: u64,
    logical_bytes: u64,
}

#[derive(Clone, Copy, Default)]
struct PhysicalAttribution {
    git_equivalent_bytes: u64,
    wheat_specific_bytes: u64,
}

impl PhysicalAttribution {
    fn total(self) -> u64 {
        self.git_equivalent_bytes + self.wheat_specific_bytes
    }
}

fn attribute_payload_region(
    stored_length: u64,
    git_logical: u64,
    wheat_logical: u64,
) -> PhysicalAttribution {
    let logical_total = git_logical + wheat_logical;
    let git_equivalent_bytes = stored_length
        .saturating_mul(git_logical)
        .checked_div(logical_total)
        .unwrap_or(0);
    PhysicalAttribution {
        git_equivalent_bytes,
        wheat_specific_bytes: stored_length - git_equivalent_bytes,
    }
}

/// Full storage analysis result.
///
/// This module only reads store files. Command dispatch currently opens the
/// store before calling here; that lifecycle behavior is outside this analysis.
struct StoreAnalysis {
    // Exhaustive regular-file census rooted at the store. This is authoritative
    // for totals; decoded categories below are explanatory subsets.
    census: FileBytes,

    // Semantic content roles across every physical content home: packed
    // segments, the selected content generation pack, and loose content.
    // Shared compressed blocks are split by each role's logical contribution,
    // never reported as logical bytes pretending to be stored bytes.
    content_role_physical: [u64; 5], // whole, fragment, overlap, required-other, unreferenced
    content_roles: [u64; 3],         // unique IDs: whole-only, fragment-only, overlap

    // Packed segments
    segment_count: usize,
    segment_errors: usize,
    segment_disk_bytes: u64,
    segment_allocated_disk_bytes: u64,
    segment_entries: u64,
    segment_logical_bytes: u64,
    segment_stored_bytes: u64,
    segment_attribution: PhysicalAttribution,
    by_kind: BTreeMap<u16, KindStats>,
    by_codec: BTreeMap<u8, CodecStats>,
    metadata_delta_depths: DeltaDepthHistogram,
    content_in_segments_count: u64,
    content_in_segments_logical: u64,
    content_in_segments_stored: u64,

    // SQLite locator
    sqlite_bytes: u64,
    sqlite_allocated_disk_bytes: u64,

    // Loose objects (metadata > threshold)
    loose_object_count: u64,
    loose_object_disk_bytes: u64,
    loose_object_allocated_disk_bytes: u64,
    loose_object_logical_bytes: u64,
    loose_by_kind: BTreeMap<u16, LooseKindStats>,
    loose_object_errors: u64,

    // Loose content (file blobs > threshold)
    loose_content_count: u64,
    loose_content_disk_bytes: u64,
    loose_content_allocated_disk_bytes: u64,

    // Content generations. The selected generation is authenticated by the
    // current catalog snapshot; all other generation files (superseded
    // generations and unrecognized debris) remain separately visible.
    content_generation_selected: FileBytes,
    content_generation_selected_pack_bytes: u64,
    content_generation_unselected: FileBytes,
}

/// Index into `content_role_physical` for one content digest:
/// whole-file only, guarded-fragment only, both, required without a semantic
/// role, or unreferenced.
fn content_role(roles: &semantic::ContentRoles, digest: &[u8; 32]) -> usize {
    let whole = roles.whole.contains(digest);
    let fragment = roles.fragment.contains(digest);
    match (whole, fragment, roles.required.contains(digest)) {
        (true, false, _) => 0,
        (false, true, _) => 1,
        (true, true, _) => 2,
        (false, false, true) => 3,
        (false, false, false) => 4,
    }
}

fn analyze_segments(storage_root: &Path, roles: &semantic::ContentRoles) -> Result<StoreAnalysis> {
    let segments_dir = storage_root.join("packed/segments");
    let mut analysis = StoreAnalysis {
        census: census_files(storage_root)?,
        content_role_physical: [0; 5],
        content_roles: [
            roles.whole_only() as u64,
            roles.fragment_only() as u64,
            roles.overlap() as u64,
        ],
        segment_count: 0,
        segment_errors: 0,
        segment_disk_bytes: 0,
        segment_allocated_disk_bytes: 0,
        segment_entries: 0,
        segment_logical_bytes: 0,
        segment_stored_bytes: 0,
        segment_attribution: PhysicalAttribution::default(),
        by_kind: BTreeMap::new(),
        by_codec: BTreeMap::new(),
        metadata_delta_depths: [0; SEGMENT_MAX_DELTA_DEPTH as usize + 1],
        content_in_segments_count: 0,
        content_in_segments_logical: 0,
        content_in_segments_stored: 0,
        sqlite_bytes: 0,
        sqlite_allocated_disk_bytes: 0,
        loose_object_count: 0,
        loose_object_disk_bytes: 0,
        loose_object_allocated_disk_bytes: 0,
        loose_object_logical_bytes: 0,
        loose_by_kind: BTreeMap::new(),
        loose_object_errors: 0,
        loose_content_count: 0,
        loose_content_disk_bytes: 0,
        loose_content_allocated_disk_bytes: 0,
        content_generation_selected: FileBytes::default(),
        content_generation_selected_pack_bytes: 0,
        content_generation_unselected: FileBytes::default(),
    };

    // --- Packed segments ---
    let active = storage_root.join("packed/active.segment");
    if segments_dir.is_dir() || active.is_file() {
        let mut segment_files: Vec<_> = if segments_dir.is_dir() {
            fs::read_dir(&segments_dir)
                .context("read segments directory")?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "segment"))
                .collect()
        } else {
            Vec::new()
        };
        if active.is_file() {
            segment_files.push(active);
        }
        segment_files.sort();

        for path in &segment_files {
            let file =
                fs::File::open(path).with_context(|| format!("open segment {}", path.display()))?;
            let reader = packed_segment_reader::open_segment(file).map_err(|error| {
                anyhow::anyhow!("authenticate segment {}: {error}", path.display())
            })?;

            let metadata = fs::metadata(path)?;
            analysis.segment_disk_bytes += metadata.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                analysis.segment_allocated_disk_bytes += metadata.blocks().saturating_mul(512);
            }
            #[cfg(not(unix))]
            {
                analysis.segment_allocated_disk_bytes += metadata.len();
            }
            analysis.segment_count += 1;

            // Track unique payload regions per segment. Block-compressed records
            // can share a region across semantic categories; allocate that region
            // proportionally to each category's logical bytes, with any rounding
            // remainder assigned to Wheat-specific storage.
            let entries = reader.entries();
            let mut payload_regions: BTreeMap<(u64, u64), (u64, u64)> = BTreeMap::new();
            let mut content_regions: BTreeMap<(u64, u64), [u64; 5]> = BTreeMap::new();
            for entry in &entries {
                let logical = payload_regions
                    .entry((entry.offset, entry.stored_length))
                    .or_default();
                if entry.namespace == 2 || is_git_equivalent_kind(entry.kind) {
                    logical.0 += entry.logical_length;
                } else {
                    logical.1 += entry.logical_length;
                }
                if entry.namespace == 2 {
                    content_regions
                        .entry((entry.offset, entry.stored_length))
                        .or_default()[content_role(roles, &entry.digest)] += entry.logical_length;
                }
            }
            let mut seen_blocks: HashSet<(u64, u64)> = HashSet::new();

            for entry in entries {
                analysis.segment_entries += 1;
                analysis.segment_logical_bytes += entry.logical_length;

                let is_block = entry.codec == 4;
                if is_block {
                    let bk = (entry.offset, entry.stored_length);
                    if seen_blocks.insert(bk) {
                        analysis.segment_stored_bytes += entry.stored_length;
                    }
                } else {
                    analysis.segment_stored_bytes += entry.stored_length;
                }

                // Content vs metadata
                let is_content = entry.namespace == 2; // SEGMENT_NAMESPACE_CONTENT
                if is_content {
                    analysis.content_in_segments_count += 1;
                    analysis.content_in_segments_logical += entry.logical_length;
                    if is_block {
                        // use logical for block entries (stored is shared)
                        analysis.content_in_segments_stored += entry.logical_length;
                    } else {
                        analysis.content_in_segments_stored += entry.stored_length;
                    }
                } else {
                    // Metadata — tally by kind
                    let stats = analysis.by_kind.entry(entry.kind).or_default();
                    stats.count += 1;
                    if entry.delta_base.is_some() {
                        stats.delta_count += 1;
                    }
                    // The reader rejects depths past the limit at open.
                    analysis.metadata_delta_depths[usize::from(entry.delta_depth)] += 1;
                    stats.logical_bytes += entry.logical_length;
                    if is_block {
                        stats.stored_bytes += entry.logical_length;
                    } else {
                        stats.stored_bytes += entry.stored_length;
                    }
                }

                // By codec
                let codec_stats = analysis.by_codec.entry(entry.codec).or_default();
                codec_stats.count += 1;
                if is_block {
                    codec_stats.stored_bytes += entry.logical_length;
                } else {
                    codec_stats.stored_bytes += entry.stored_length;
                }
            }

            let mut payload_bytes = 0u64;
            for ((_, stored_length), (git_logical, wheat_logical)) in payload_regions {
                payload_bytes += stored_length;
                let attribution =
                    attribute_payload_region(stored_length, git_logical, wheat_logical);
                analysis.segment_attribution.git_equivalent_bytes +=
                    attribution.git_equivalent_bytes;
                analysis.segment_attribution.wheat_specific_bytes +=
                    attribution.wheat_specific_bytes;
            }
            analysis.segment_attribution.wheat_specific_bytes +=
                metadata.len().saturating_sub(payload_bytes);
            for ((_, stored), logical) in content_regions {
                let total: u64 = logical.iter().sum();
                if total == 0 {
                    analysis.content_role_physical[4] += stored;
                    continue;
                }
                let mut attributed = 0;
                for (role, logical_bytes) in logical.iter().enumerate().take(4) {
                    let bytes = stored.saturating_mul(*logical_bytes) / total;
                    analysis.content_role_physical[role] += bytes;
                    attributed += bytes;
                }
                analysis.content_role_physical[4] += stored - attributed;
            }
        }
    }

    // --- SQLite locator ---
    let sqlite_path = storage_root.join("packed/catalog-locator.sqlite3");
    if sqlite_path.is_file() {
        let mut bytes = FileBytes::default();
        bytes.add_metadata(&fs::metadata(&sqlite_path)?);
        analysis.sqlite_bytes = bytes.apparent_bytes;
        analysis.sqlite_allocated_disk_bytes = bytes.allocated_bytes;
    }

    // --- Content generations ---
    let generations_dir = storage_root.join("content-generations");
    if generations_dir.is_dir() {
        walk_content_generations(
            &generations_dir,
            selected_content_generation(storage_root),
            roles,
            &mut analysis,
        )?;
    }

    // --- Loose objects (metadata) ---
    let loose_objects_dir = storage_root.join("loose-objects");
    if loose_objects_dir.is_dir() {
        walk_loose_objects(&loose_objects_dir, &mut analysis)?;
    }

    // --- Loose content (file blobs) ---
    let loose_content_dir = storage_root.join("loose-content");
    if loose_content_dir.is_dir() {
        walk_loose_content(&loose_content_dir, roles, &mut analysis)?;
    }

    Ok(analysis)
}

fn walk_loose_objects(root: &Path, analysis: &mut StoreAnalysis) -> Result<()> {
    for bucket in fs::read_dir(root).context("read loose-objects")? {
        let bucket = bucket?;
        if !bucket.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(bucket.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            let disk_bytes = metadata.len();
            analysis.loose_object_count += 1;
            analysis.loose_object_disk_bytes += disk_bytes;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                analysis.loose_object_allocated_disk_bytes += metadata.blocks().saturating_mul(512);
            }
            #[cfg(not(unix))]
            {
                analysis.loose_object_allocated_disk_bytes += disk_bytes;
            }

            // Read the file to determine its canonical kind and logical size.
            match read_loose_object_kind(&entry.path()) {
                Ok((kind_code, logical_len)) => {
                    analysis.loose_object_logical_bytes += logical_len;
                    let stats = analysis.loose_by_kind.entry(kind_code).or_default();
                    stats.count += 1;
                    stats.disk_bytes += disk_bytes;
                    stats.logical_bytes += logical_len;
                }
                Err(_) => {
                    analysis.loose_object_errors += 1;
                }
            }
        }
    }
    Ok(())
}

fn read_loose_object_kind(path: &Path) -> Result<(u16, u64)> {
    let canonical_bytes =
        read_loose_envelope(path).map_err(|e| anyhow::anyhow!("read envelope: {e}"))?;
    let logical_len = canonical_bytes.len() as u64;
    let kind: ObjectKind = validate_canonical_object(&canonical_bytes)?.into();
    Ok((kind as u16, logical_len))
}

fn walk_loose_content(
    root: &Path,
    roles: &semantic::ContentRoles,
    analysis: &mut StoreAnalysis,
) -> Result<()> {
    for bucket in fs::read_dir(root).context("read loose-content")? {
        let bucket = bucket?;
        if !bucket.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(bucket.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            analysis.loose_content_count += 1;
            analysis.loose_content_disk_bytes += metadata.len();
            // Loose content lives at `<first two hex>/<remaining 62 hex>` of
            // its ContentId. Anything else (temporary files, debris) has no
            // content identity and is charged as unreferenced.
            let role = loose_content_digest(&bucket.file_name(), &entry.file_name())
                .map_or(4, |digest| content_role(roles, &digest));
            analysis.content_role_physical[role] += metadata.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                analysis.loose_content_allocated_disk_bytes +=
                    metadata.blocks().saturating_mul(512);
            }
            #[cfg(not(unix))]
            {
                analysis.loose_content_allocated_disk_bytes += metadata.len();
            }
        }
    }
    Ok(())
}

fn loose_content_digest(bucket: &std::ffi::OsStr, name: &std::ffi::OsStr) -> Option<[u8; 32]> {
    let id = format!("{}{}", bucket.to_str()?, name.to_str()?);
    id.parse::<ContentId>().ok().map(|id| *id.as_bytes())
}

/// Attribute the selected generation's shard pack bytes to content roles.
///
/// The index is authenticated by the catalog-selected digest and each shard
/// manifest by the digest the index names; every byte of a shard pack belongs
/// to exactly one of that shard's entries, so per-entry physical lengths split
/// the packs exactly.
fn attribute_selected_generation(
    root: &Path,
    selected: SelectedContentGeneration,
    roles: &semantic::ContentRoles,
    analysis: &mut StoreAnalysis,
) -> Result<()> {
    let manifest = read_content_generation(
        root,
        PhysicalContentGeneration {
            generation: selected.generation,
            manifest: selected.manifest,
        },
    )
    .map_err(|error| anyhow::anyhow!("read selected content generation: {error}"))?;
    for shard in 0..manifest.shard_count() {
        let pack = content_generation_shard_pack_path(root, selected.generation, shard);
        let pack_length = fs::metadata(&pack)
            .with_context(|| format!("stat selected content pack {}", pack.display()))?
            .len();
        // Entries are ordered by ID; the pack is laid out in its own order.
        // Every pack byte must belong to exactly one entry for the split to be
        // exact.
        let mut extents: Vec<(u64, u64)> = manifest
            .shard_entries(shard)
            .iter()
            .map(|entry| (entry.pack_offset, entry.physical_length))
            .collect();
        extents.sort_unstable();
        let mut expected_offset = 0u64;
        for (offset, length) in extents {
            if offset != expected_offset {
                anyhow::bail!("selected content manifest entries are not contiguous");
            }
            expected_offset = expected_offset
                .checked_add(length)
                .context("selected content manifest length overflow")?;
        }
        if expected_offset != pack_length {
            anyhow::bail!(
                "selected content pack {} is {pack_length} bytes but its manifest describes {expected_offset}",
                pack.display()
            );
        }
    }
    for entry in &manifest.entries {
        analysis.content_role_physical[content_role(roles, entry.id.as_bytes())] +=
            entry.physical_length;
    }
    Ok(())
}

fn walk_content_generations(
    root: &Path,
    selected: Option<SelectedContentGeneration>,
    roles: &semantic::ContentRoles,
    analysis: &mut StoreAnalysis,
) -> Result<()> {
    if let Some(selected) = selected {
        attribute_selected_generation(root, selected, roles, analysis)?;
    }
    for entry in fs::read_dir(root).context("read content-generations")? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            let is_selected_manifest = selected.is_some_and(|selected| {
                entry.file_name().to_string_lossy()
                    == format!(
                        "{}-{}.manifest",
                        selected.generation,
                        hex_encode(&selected.manifest)
                    )
            });
            let bytes = if is_selected_manifest {
                &mut analysis.content_generation_selected
            } else {
                &mut analysis.content_generation_unselected
            };
            bytes.add_metadata(&entry.metadata()?);
        } else if file_type.is_dir() {
            let is_selected_generation = selected.is_some_and(|selected| {
                entry.file_name().to_string_lossy() == selected.generation.to_string()
            });
            let bytes = if is_selected_generation {
                &mut analysis.content_generation_selected
            } else {
                &mut analysis.content_generation_unselected
            };
            let selected_pack_bytes = walk_content_generation_files(&entry.path(), bytes)?;
            if is_selected_generation {
                analysis.content_generation_selected_pack_bytes += selected_pack_bytes;
            }
        }
    }
    Ok(())
}

fn walk_content_generation_files(root: &Path, bytes: &mut FileBytes) -> Result<u64> {
    let mut content_pack_bytes = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            let metadata = entry.metadata()?;
            bytes.add_metadata(&metadata);
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "pack")
            {
                content_pack_bytes += metadata.len();
            }
        } else if file_type.is_dir() {
            content_pack_bytes += walk_content_generation_files(&entry.path(), bytes)?;
        }
    }
    Ok(content_pack_bytes)
}

fn census_files(root: &Path) -> Result<FileBytes> {
    let mut total = FileBytes::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("read store directory {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total.add_metadata(&entry.metadata()?);
            }
        }
    }
    Ok(total)
}

/// Format the analysis as a human-readable report.
fn apparent_total(a: &StoreAnalysis) -> u64 {
    a.census.apparent_bytes
}

fn allocated_total(a: &StoreAnalysis) -> u64 {
    a.census.allocated_bytes
}

/// Attribute every apparent byte counted by this analyzer exactly once.
///
/// Git-equivalent bytes are file contents, trees, commit-like Revisions, and
/// ref-like ViewLabelManifests. All richer Wheat model objects plus physical
/// manifests, indexes, framing, and unselected data are Wheat-specific.
/// Loose-object envelope compression is charged according
/// to the decoded object's kind; selected generation manifests remain
/// Wheat-specific physical metadata because their entries are not file content.
fn physical_attribution(a: &StoreAnalysis) -> PhysicalAttribution {
    let loose_git = a
        .loose_by_kind
        .iter()
        .filter(|(kind, _)| is_git_equivalent_kind(**kind))
        .map(|(_, stats)| stats.disk_bytes)
        .sum();
    let selected_manifest_bytes = a
        .content_generation_selected
        .apparent_bytes
        .saturating_sub(a.content_generation_selected_pack_bytes);
    let git_equivalent_bytes = a.segment_attribution.git_equivalent_bytes
        + loose_git
        + a.loose_content_disk_bytes
        + a.content_generation_selected_pack_bytes;
    let categorized_wheat = a.segment_attribution.wheat_specific_bytes
        + a.loose_object_disk_bytes.saturating_sub(loose_git)
        + a.sqlite_bytes
        + selected_manifest_bytes
        + a.content_generation_unselected.apparent_bytes;
    let categorized = git_equivalent_bytes.saturating_add(categorized_wheat);
    // Catalog/head/active/root sidecars and unknown debris are real physical
    // bytes. They are Wheat-specific overhead rather than silently omitted.
    let wheat_specific_bytes =
        categorized_wheat.saturating_add(a.census.apparent_bytes.saturating_sub(categorized));
    PhysicalAttribution {
        git_equivalent_bytes,
        wheat_specific_bytes,
    }
}

fn metadata_delta_records(depths: &DeltaDepthHistogram) -> u64 {
    depths[1..].iter().sum()
}

fn max_metadata_delta_depth(depths: &DeltaDepthHistogram) -> usize {
    depths.iter().rposition(|count| *count > 0).unwrap_or(0)
}

/// One line: delta count, deepest chain, and the non-empty depth buckets.
fn format_delta_depths(depths: &DeltaDepthHistogram) -> String {
    let buckets = depths
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(depth, count)| format!("{depth}:{count}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "metadata delta chains: {} deltas, max depth {}; entries by depth [{}]",
        metadata_delta_records(depths),
        max_metadata_delta_depth(depths),
        buckets,
    )
}

fn format_report(a: &StoreAnalysis) -> String {
    let mut out = String::new();

    let total_disk = apparent_total(a);
    let total_allocated_disk = allocated_total(a);

    writeln!(out, "wheat store analysis").unwrap();
    writeln!(
        out,
        "═══════════════════════════════════════════════════════════════"
    )
    .unwrap();
    writeln!(out).unwrap();

    // Grand total
    writeln!(
        out,
        "total on disk (apparent):  {}",
        format_bytes(total_disk)
    )
    .unwrap();
    writeln!(
        out,
        "total on disk (allocated): {}",
        format_bytes(total_allocated_disk)
    )
    .unwrap();
    writeln!(out).unwrap();

    let attribution = physical_attribution(a);
    writeln!(out, "physical attribution (apparent bytes, counted once)").unwrap();
    writeln!(
        out,
        "  Git-equivalent (content + trees + revisions + refs): {}",
        format_bytes(attribution.git_equivalent_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  Wheat-specific (model + physical overhead): {}",
        format_bytes(attribution.wheat_specific_bytes)
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "semantic content roles (retained authenticated roots)").unwrap();
    writeln!(
        out,
        "  whole-file only:         {} IDs, {} physical",
        a.content_roles[0],
        format_bytes(a.content_role_physical[0])
    )
    .unwrap();
    writeln!(
        out,
        "  guarded-fragment only:   {} IDs, {} physical",
        a.content_roles[1],
        format_bytes(a.content_role_physical[1])
    )
    .unwrap();
    writeln!(
        out,
        "  overlap (both roles):    {} IDs, {} physical",
        a.content_roles[2],
        format_bytes(a.content_role_physical[2])
    )
    .unwrap();
    writeln!(
        out,
        "  required aggregate, no semantic role: {}",
        format_bytes(a.content_role_physical[3])
    )
    .unwrap();
    writeln!(
        out,
        "  truly unreferenced content: {}",
        format_bytes(a.content_role_physical[4])
    )
    .unwrap();
    writeln!(out).unwrap();

    // Top-level breakdown
    writeln!(out, "  {:<28} {:>10}  {:>5}", "Category", "Size", "%").unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>5}",
        "────────────────────────────", "──────────", "─────"
    )
    .unwrap();
    let pct = |n: u64| -> String {
        if total_disk == 0 {
            "0.0".to_string()
        } else {
            format!("{:.1}", n as f64 / total_disk as f64 * 100.0)
        }
    };
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "Packed segments",
        format_bytes(a.segment_disk_bytes),
        pct(a.segment_disk_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "Loose objects (metadata)",
        format_bytes(a.loose_object_disk_bytes),
        pct(a.loose_object_disk_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "Loose content (file blobs)",
        format_bytes(a.loose_content_disk_bytes),
        pct(a.loose_content_disk_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "SQLite locator index",
        format_bytes(a.sqlite_bytes),
        pct(a.sqlite_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "Selected content generation",
        format_bytes(a.content_generation_selected.apparent_bytes),
        pct(a.content_generation_selected.apparent_bytes)
    )
    .unwrap();
    writeln!(
        out,
        "  {:<28} {:>10}  {:>4}%",
        "Unselected generations",
        format_bytes(a.content_generation_unselected.apparent_bytes),
        pct(a.content_generation_unselected.apparent_bytes)
    )
    .unwrap();
    writeln!(out).unwrap();

    // Packed segments detail
    writeln!(out, "packed segments").unwrap();
    writeln!(
        out,
        "───────────────────────────────────────────────────────────────"
    )
    .unwrap();
    writeln!(
        out,
        "  segments: {}  entries: {}  errors: {}",
        a.segment_count, a.segment_entries, a.segment_errors
    )
    .unwrap();
    writeln!(
        out,
        "  disk: {}  logical: {}  stored: {}",
        format_bytes(a.segment_disk_bytes),
        format_bytes(a.segment_logical_bytes),
        format_bytes(a.segment_stored_bytes),
    )
    .unwrap();
    if a.segment_logical_bytes > 0 {
        writeln!(
            out,
            "  compression ratio: {:.2}x (logical / disk)",
            a.segment_logical_bytes as f64 / a.segment_disk_bytes as f64
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    // Content in segments
    writeln!(
        out,
        "  content in segments: {} files  logical: {}  stored: {}",
        a.content_in_segments_count,
        format_bytes(a.content_in_segments_logical),
        format_bytes(a.content_in_segments_stored),
    )
    .unwrap();
    writeln!(out).unwrap();

    // Metadata by kind (sorted by logical bytes descending)
    writeln!(
        out,
        "  metadata in segments (by kind, sorted by logical size):"
    )
    .unwrap();
    writeln!(
        out,
        "  {:<25} {:>8} {:>8} {:>12} {:>12} {:>8}",
        "Kind", "Count", "Deltas", "Logical", "Stored", "Avg"
    )
    .unwrap();
    writeln!(
        out,
        "  {:<25} {:>8} {:>8} {:>12} {:>12} {:>8}",
        "─────────────────────────",
        "────────",
        "────────",
        "────────────",
        "────────────",
        "────────"
    )
    .unwrap();
    let mut kinds: Vec<_> = a.by_kind.iter().collect();
    kinds.sort_by_key(|(_, v)| std::cmp::Reverse(v.logical_bytes));
    for (kind, stats) in &kinds {
        let avg = stats.logical_bytes.checked_div(stats.count).unwrap_or(0);
        writeln!(
            out,
            "  {:<25} {:>8} {:>8} {:>12} {:>12} {:>8}",
            kind_name(**kind),
            stats.count,
            stats.delta_count,
            format_bytes(stats.logical_bytes),
            format_bytes(stats.stored_bytes),
            format_bytes(avg),
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "  {}", format_delta_depths(&a.metadata_delta_depths)).unwrap();
    writeln!(out).unwrap();

    // By codec
    writeln!(out, "  by codec:").unwrap();
    writeln!(out, "  {:<12} {:>10} {:>12}", "Codec", "Count", "Stored").unwrap();
    for (codec, stats) in &a.by_codec {
        writeln!(
            out,
            "  {:<12} {:>10} {:>12}",
            codec_name(*codec),
            stats.count,
            format_bytes(stats.stored_bytes),
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "content generations").unwrap();
    writeln!(
        out,
        "───────────────────────────────────────────────────────────────"
    )
    .unwrap();
    writeln!(
        out,
        "  selected (current catalog): apparent {}  allocated {}",
        format_bytes(a.content_generation_selected.apparent_bytes),
        format_bytes(a.content_generation_selected.allocated_bytes),
    )
    .unwrap();
    writeln!(
        out,
        "  unselected: apparent {}  allocated {}",
        format_bytes(a.content_generation_unselected.apparent_bytes),
        format_bytes(a.content_generation_unselected.allocated_bytes),
    )
    .unwrap();
    writeln!(
        out,
        "  Unselected includes every content-generation file the current catalog does not select: superseded generations and unrecognized debris."
    )
    .unwrap();
    writeln!(out).unwrap();

    // Loose objects detail
    if a.loose_object_count > 0 {
        writeln!(out, "loose objects (metadata > 128 KiB threshold)").unwrap();
        writeln!(
            out,
            "───────────────────────────────────────────────────────────────"
        )
        .unwrap();
        writeln!(
            out,
            "  files: {}  disk: {}  logical: {}  errors: {}",
            a.loose_object_count,
            format_bytes(a.loose_object_disk_bytes),
            format_bytes(a.loose_object_logical_bytes),
            a.loose_object_errors,
        )
        .unwrap();
        if a.loose_object_count > 0 {
            writeln!(
                out,
                "  avg disk: {}  overhead vs logical: {:.0}%",
                format_bytes(
                    a.loose_object_disk_bytes
                        .checked_div(a.loose_object_count)
                        .unwrap_or(0)
                ),
                if a.loose_object_logical_bytes > 0 {
                    (a.loose_object_disk_bytes as f64 / a.loose_object_logical_bytes as f64 - 1.0)
                        * 100.0
                } else {
                    0.0
                }
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        // By kind
        writeln!(
            out,
            "  {:<25} {:>8} {:>12} {:>12} {:>8}",
            "Kind", "Count", "Disk", "Logical", "Avg"
        )
        .unwrap();
        writeln!(
            out,
            "  {:<25} {:>8} {:>12} {:>12} {:>8}",
            "─────────────────────────", "────────", "────────────", "────────────", "────────"
        )
        .unwrap();
        let mut loose_kinds: Vec<_> = a.loose_by_kind.iter().collect();
        loose_kinds.sort_by_key(|(_, v)| std::cmp::Reverse(v.disk_bytes));
        for (kind, stats) in &loose_kinds {
            let avg = stats.logical_bytes.checked_div(stats.count).unwrap_or(0);
            writeln!(
                out,
                "  {:<25} {:>8} {:>12} {:>12} {:>8}",
                kind_name(**kind),
                stats.count,
                format_bytes(stats.disk_bytes),
                format_bytes(stats.logical_bytes),
                format_bytes(avg),
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }

    // Loose content detail
    if a.loose_content_count > 0 {
        writeln!(out, "loose content (file blobs > 128 KiB threshold)").unwrap();
        writeln!(
            out,
            "───────────────────────────────────────────────────────────────"
        )
        .unwrap();
        writeln!(
            out,
            "  files: {}  disk: {}",
            a.loose_content_count,
            format_bytes(a.loose_content_disk_bytes),
        )
        .unwrap();
        writeln!(
            out,
            "  avg: {}",
            format_bytes(
                a.loose_content_disk_bytes
                    .checked_div(a.loose_content_count)
                    .unwrap_or(0)
            ),
        )
        .unwrap();
        writeln!(out).unwrap();
    }

    out
}

pub(crate) fn analyze(store_root: &Path) -> Result<crate::command_output::Success> {
    let storage_root = store_root.join("storage");
    if !storage_root.is_dir() {
        anyhow::bail!(
            "no storage directory at {}; is this a packed wheat store?",
            storage_root.display()
        );
    }

    let roles = semantic::classify(store_root)?;
    let mut analysis = analyze_segments(&storage_root, &roles)?;
    analysis.census = census_files(store_root)?;
    let human = format_report(&analysis);

    let total_disk = apparent_total(&analysis);
    let total_allocated_disk = allocated_total(&analysis);
    let attribution = physical_attribution(&analysis);
    debug_assert_eq!(attribution.total(), total_disk);

    crate::command_output::Success::new(
        "store.analyze",
        serde_json::json!({
            "total_disk_bytes": total_disk,
            "total_allocated_disk_bytes": total_allocated_disk,
            "git_equivalent_bytes": attribution.git_equivalent_bytes,
            "wheat_specific_bytes": attribution.wheat_specific_bytes,
            "content_roles": {
                "whole_file_only_ids": analysis.content_roles[0],
                "guarded_fragment_only_ids": analysis.content_roles[1],
                "whole_file_and_guarded_fragment_ids": analysis.content_roles[2],
                "whole_file_only_physical_bytes": analysis.content_role_physical[0],
                "guarded_fragment_only_physical_bytes": analysis.content_role_physical[1],
                "whole_file_and_guarded_fragment_physical_bytes": analysis.content_role_physical[2],
                "required_other_physical_bytes": analysis.content_role_physical[3],
                "unreferenced_physical_bytes": analysis.content_role_physical[4],
            },
            "segment_disk_bytes": analysis.segment_disk_bytes,
            "segment_allocated_disk_bytes": analysis.segment_allocated_disk_bytes,
            "segment_count": analysis.segment_count,
            "segment_entries": analysis.segment_entries,
            "segment_logical_bytes": analysis.segment_logical_bytes,
            "metadata_delta_records": metadata_delta_records(&analysis.metadata_delta_depths),
            "metadata_max_delta_depth": max_metadata_delta_depth(&analysis.metadata_delta_depths),
            "metadata_entries_by_delta_depth": analysis.metadata_delta_depths,
            "loose_object_count": analysis.loose_object_count,
            "loose_object_disk_bytes": analysis.loose_object_disk_bytes,
            "loose_object_allocated_disk_bytes": analysis.loose_object_allocated_disk_bytes,
            "loose_content_count": analysis.loose_content_count,
            "loose_content_disk_bytes": analysis.loose_content_disk_bytes,
            "loose_content_allocated_disk_bytes": analysis.loose_content_allocated_disk_bytes,
            "sqlite_bytes": analysis.sqlite_bytes,
            "sqlite_allocated_disk_bytes": analysis.sqlite_allocated_disk_bytes,
            "content_generation_selected_bytes": analysis.content_generation_selected.apparent_bytes,
            "content_generation_selected_allocated_disk_bytes": analysis.content_generation_selected.allocated_bytes,
            "content_generation_unselected_bytes": analysis.content_generation_unselected.apparent_bytes,
            "content_generation_unselected_allocated_disk_bytes": analysis.content_generation_unselected.allocated_bytes,
        }),
        human,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_equivalent_kind_mapping_covers_git_semantics_only() {
        assert!(is_git_equivalent_kind(ObjectKind::Tree as u16));
        assert!(is_git_equivalent_kind(ObjectKind::Revision as u16));
        assert!(is_git_equivalent_kind(ObjectKind::ViewLabelManifest as u16));
        assert!(!is_git_equivalent_kind(ObjectKind::Change as u16));
        assert!(!is_git_equivalent_kind(ObjectKind::Application as u16));
    }

    #[test]
    fn mixed_payload_region_is_counted_once_and_split_by_logical_share() {
        let attribution = attribute_payload_region(101, 3, 1);
        assert_eq!(attribution.git_equivalent_bytes, 75);
        assert_eq!(attribution.wheat_specific_bytes, 26);
        assert_eq!(attribution.total(), 101);
    }

    #[test]
    fn delta_depth_summary_counts_only_deltas_and_names_the_deepest_chain() {
        let mut depths = [0; SEGMENT_MAX_DELTA_DEPTH as usize + 1];
        assert_eq!(
            format_delta_depths(&depths),
            "metadata delta chains: 0 deltas, max depth 0; entries by depth []"
        );
        depths[0] = 5;
        depths[1] = 3;
        depths[16] = 1;
        assert_eq!(metadata_delta_records(&depths), 4);
        assert_eq!(max_metadata_delta_depth(&depths), 16);
        assert_eq!(
            format_delta_depths(&depths),
            "metadata delta chains: 4 deltas, max depth 16; entries by depth [0:5 1:3 16:1]"
        );
    }

    #[test]
    fn zero_logical_payload_region_is_wheat_specific() {
        let attribution = attribute_payload_region(17, 0, 0);
        assert_eq!(attribution.git_equivalent_bytes, 0);
        assert_eq!(attribution.wheat_specific_bytes, 17);
    }

    fn write_v2_catalog(storage: &Path, content: PhysicalContentGeneration) {
        let snapshot = PhysicalCatalogSnapshot {
            generation: 1,
            segments: Vec::new(),
            content,
        };
        let catalog = encode_physical_catalog_snapshot(&snapshot).unwrap();
        let digest = *blake3::hash(&catalog).as_bytes();
        let packed = storage.join("packed");
        fs::create_dir_all(packed.join("catalog")).unwrap();
        fs::write(
            packed
                .join("catalog")
                .join(format!("1-{}.catalog", hex_encode(&digest))),
            catalog,
        )
        .unwrap();
        let mut head = b"WHEATPACK".to_vec();
        head.push(1);
        head.extend_from_slice(&1u64.to_be_bytes());
        head.extend_from_slice(&digest);
        fs::write(packed.join("catalog.head"), head).unwrap();
    }

    /// Write a real, sealed content generation holding `pack` as one raw
    /// entry, select it in the catalog snapshot, and return the manifest length.
    fn write_selected_generation(storage: &Path, generation: u64, pack: &[u8]) -> u64 {
        let entries = vec![wheat_store::ContentGenerationEntry {
            id: ContentId::of(pack),
            envelope_digest: [0; 32],
            pack_offset: 0,
            physical_length: pack.len() as u64,
            logical_length: pack.len() as u64,
            delta_base: None,
            delta_depth: 0,
        }];
        let manifest = ContentGenerationManifest::new(generation, entries).unwrap();
        let (_, manifest_bytes) = write_generation(storage, &manifest, &[pack]);
        manifest_bytes
    }

    /// Write `manifest`'s index, shard manifests, and shard packs (`packs[i]`
    /// for shard `i`), select it in the catalog, and return the index digest
    /// and the total index + shard-manifest bytes.
    fn write_generation(
        storage: &Path,
        manifest: &ContentGenerationManifest,
        packs: &[&[u8]],
    ) -> ([u8; 32], u64) {
        let generations = storage.join("content-generations");
        let generation = manifest.generation;
        fs::create_dir_all(generations.join(generation.to_string())).unwrap();
        let mut manifest_bytes = 0;
        for (shard, pack) in packs.iter().enumerate() {
            let encoded = manifest.encode_shard(shard).unwrap();
            manifest_bytes += encoded.len() as u64;
            fs::write(
                content_generation_shard_manifest_path(&generations, generation, shard),
                encoded,
            )
            .unwrap();
            fs::write(
                content_generation_shard_pack_path(&generations, generation, shard),
                pack,
            )
            .unwrap();
        }
        let encoded = manifest.index().unwrap().encoded().unwrap();
        manifest_bytes += encoded.len() as u64;
        let digest = *blake3::hash(&encoded).as_bytes();
        fs::write(
            generations.join(format!("{generation}-{}.manifest", hex_encode(&digest))),
            &encoded,
        )
        .unwrap();
        write_v2_catalog(
            storage,
            PhysicalContentGeneration {
                generation,
                manifest: digest,
            },
        );
        (digest, manifest_bytes)
    }

    #[test]
    fn accounts_selected_and_unselected_content_generation_files_once() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let manifest_bytes = write_selected_generation(&storage, 7, b"selected-pack");

        let generations = storage.join("content-generations");
        fs::create_dir_all(generations.join("6")).unwrap();
        fs::write(generations.join("6/content.pack"), b"old-pack").unwrap();
        fs::write(generations.join("operator-notes"), b"debris").unwrap();

        let analysis = analyze_segments(&storage, &semantic::ContentRoles::default()).unwrap();
        let selected_bytes = b"selected-pack".len() as u64 + manifest_bytes;
        let unselected_bytes = (b"old-pack".len() + b"debris".len()) as u64;
        // The fixture's catalog snapshot and catalog.head sidecar are counted by
        // the census but belong to neither generation bucket.
        let catalog_path = storage
            .join("packed/catalog")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let sidecar_bytes = (fs::metadata(&catalog_path).unwrap().len()
            + fs::metadata(storage.join("packed/catalog.head"))
                .unwrap()
                .len()) as u64;

        assert_eq!(
            analysis.content_generation_selected.apparent_bytes,
            selected_bytes
        );
        assert_eq!(
            analysis.content_generation_unselected.apparent_bytes,
            unselected_bytes
        );
        assert_eq!(
            apparent_total(&analysis),
            selected_bytes + unselected_bytes + sidecar_bytes
        );
        assert!(
            allocated_total(&analysis) >= apparent_total(&analysis),
            "allocated disk bytes must include generation files"
        );

        let report = format_report(&analysis);
        assert!(report.contains("selected (current catalog)"));
        assert!(report.contains("unselected: apparent"));
    }

    #[test]
    fn zero_numbered_generation_is_selected_when_manifest_is_nonzero() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let manifest_bytes = write_selected_generation(&storage, 0, b"first-generation-pack");
        let analysis = analyze_segments(&storage, &semantic::ContentRoles::default()).unwrap();
        assert_eq!(
            analysis.content_generation_selected.apparent_bytes,
            b"first-generation-pack".len() as u64 + manifest_bytes
        );
        // No retained roots in this fixture: the whole pack is unreferenced.
        assert_eq!(
            analysis.content_role_physical,
            [0, 0, 0, 0, b"first-generation-pack".len() as u64]
        );
        assert_eq!(analysis.content_generation_unselected.apparent_bytes, 0);
    }

    #[test]
    fn json_includes_generation_and_allocated_disk_accounting() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let manifest_bytes = write_selected_generation(&storage, 2, b"pack");

        let result = analyze(temp.path()).unwrap().result;
        assert_eq!(
            result["content_generation_selected_bytes"],
            4 + manifest_bytes
        );
        assert_eq!(result["content_generation_unselected_bytes"], 0);
        assert_eq!(result["git_equivalent_bytes"], 4);
        // Manifest + the fixture's catalog snapshot and catalog.head sidecars.
        let catalog_bytes = fs::metadata(
            storage
                .join("packed/catalog")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap()
        .len();
        let head_bytes = fs::metadata(storage.join("packed/catalog.head"))
            .unwrap()
            .len();
        assert_eq!(
            result["wheat_specific_bytes"],
            manifest_bytes + catalog_bytes + head_bytes
        );
        assert_eq!(
            result["git_equivalent_bytes"].as_u64().unwrap()
                + result["wheat_specific_bytes"].as_u64().unwrap(),
            result["total_disk_bytes"].as_u64().unwrap()
        );
        assert!(result["total_allocated_disk_bytes"].as_u64().is_some());
        assert!(
            result["content_generation_selected_allocated_disk_bytes"]
                .as_u64()
                .is_some()
        );
    }

    /// Manifest entries are ID-ordered while the pack has its own layout; the
    /// exactness check must follow pack offsets, and roles follow IDs.
    #[test]
    fn selected_pack_split_follows_offsets_not_id_order() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let mut ids = [ContentId::of(b"alpha"), ContentId::of(b"beta")];
        ids.sort();
        // The larger ID sits first in the pack.
        let entries = vec![
            wheat_store::ContentGenerationEntry {
                id: ids[0],
                envelope_digest: [0; 32],
                pack_offset: 7,
                physical_length: 3,
                logical_length: 3,
                delta_base: None,
                delta_depth: 0,
            },
            wheat_store::ContentGenerationEntry {
                id: ids[1],
                envelope_digest: [0; 32],
                pack_offset: 0,
                physical_length: 7,
                logical_length: 7,
                delta_base: None,
                delta_depth: 0,
            },
        ];
        let manifest = ContentGenerationManifest::new(4, entries).unwrap();
        write_generation(&storage, &manifest, &[b"0123456789"]);
        let roles = semantic::ContentRoles {
            whole: HashSet::from([*ids[0].as_bytes()]),
            fragment: HashSet::from([*ids[1].as_bytes()]),
            ..Default::default()
        };
        let analysis = analyze_segments(&storage, &roles).unwrap();
        assert_eq!(analysis.content_role_physical, [3, 7, 0, 0, 0]);
    }

    /// A generation spanning several shards: every shard pack is checked
    /// against its own entries and all of them are attributed.
    #[test]
    fn multi_shard_generation_splits_every_shard_pack() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let count = wheat_store::CONTENT_GENERATION_SHARD_ENTRIES + 2;
        let mut ids = (0..count as u64)
            .map(|n| ContentId::of(&n.to_be_bytes()))
            .collect::<Vec<_>>();
        ids.sort();
        let entries = ids
            .iter()
            .enumerate()
            .map(|(index, id)| wheat_store::ContentGenerationEntry {
                id: *id,
                envelope_digest: [0; 32],
                pack_offset: (index % wheat_store::CONTENT_GENERATION_SHARD_ENTRIES) as u64,
                physical_length: 1,
                logical_length: 1,
                delta_base: None,
                delta_depth: 0,
            })
            .collect();
        let manifest = ContentGenerationManifest::new(5, entries).unwrap();
        assert_eq!(manifest.shard_count(), 2);
        let first = vec![b'x'; wheat_store::CONTENT_GENERATION_SHARD_ENTRIES];
        let (_, manifest_bytes) = write_generation(&storage, &manifest, &[&first, b"yz"]);

        let analysis = analyze_segments(&storage, &semantic::ContentRoles::default()).unwrap();
        assert_eq!(analysis.content_role_physical, [0, 0, 0, 0, count as u64]);
        assert_eq!(
            analysis.content_generation_selected_pack_bytes,
            count as u64
        );
        assert_eq!(
            analysis.content_generation_selected.apparent_bytes,
            count as u64 + manifest_bytes
        );

        fs::write(
            storage.join("content-generations/5/shard-000001.pack"),
            b"yz-and-more",
        )
        .unwrap();
        let error = analyze_segments(&storage, &semantic::ContentRoles::default())
            .err()
            .expect("a second-shard pack larger than its entries must be rejected");
        assert!(error.to_string().contains("manifest describes"), "{error}");
    }

    #[test]
    fn selected_pack_length_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        write_selected_generation(&storage, 3, b"pack");
        fs::write(
            storage.join("content-generations/3/shard-000000.pack"),
            b"pack-with-trailing-bytes",
        )
        .unwrap();
        let error = analyze_segments(&storage, &semantic::ContentRoles::default())
            .err()
            .expect("a pack larger than its manifest must not produce an exact split");
        assert!(error.to_string().contains("manifest describes"), "{error}");
    }

    #[test]
    fn treats_generation_files_as_unselected_when_catalog_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let generations = storage.join("content-generations");
        fs::create_dir_all(generations.join("1")).unwrap();
        fs::write(generations.join("1/content.pack"), b"stray-pack").unwrap();
        fs::write(generations.join("1-deadbeef.manifest"), b"stray-manifest").unwrap();

        let analysis = analyze_segments(&storage, &semantic::ContentRoles::default()).unwrap();
        let expected = (b"stray-pack".len() + b"stray-manifest".len()) as u64;
        assert_eq!(analysis.content_generation_selected.apparent_bytes, 0);
        assert_eq!(
            analysis.content_generation_unselected.apparent_bytes,
            expected
        );
    }
}
