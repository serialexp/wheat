//! `wheat store analyze` — storage breakdown for a wheat repository.
//!
//! Walks packed segments, loose objects, and loose content to produce a
//! table showing where bytes live and what kinds of data they hold.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use wheat_core::validate_canonical_object;
use wheat_store::ObjectKind;
use wheat_store::packed_segment_reader::{self, read_loose_envelope};

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
            ObjectKind::ContentManifest => "ContentManifest",
            ObjectKind::RepositoryState => "RepositoryState",
            ObjectKind::RevisionMapNode => "RevisionMapNode",
            ObjectKind::ApplicationMapNode => "ApplicationMapNode",
            ObjectKind::ConflictSetNode => "ConflictSetNode",
            ObjectKind::ContentSetNode => "ContentSetNode",
            ObjectKind::ContentSetSnapshot => "ContentSetSnapshot",
            ObjectKind::ContentSetDelta => "ContentSetDelta",
            ObjectKind::RetainedSetNode => "RetainedSetNode",
            ObjectKind::ViewLabelManifest => "ViewLabelManifest",
            ObjectKind::RepositoryEnvelope => "Envelope",
            ObjectKind::Edit => "Edit",
            ObjectKind::ChangeMapNode => "ChangeMapNode",
            ObjectKind::ViewLineageMapNode => "ViewLineageMapNode",
        })
        .unwrap_or("Unknown")
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

/// Per-kind accumulator for segment entries.
#[derive(Default)]
struct KindStats {
    count: u64,
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

/// Full storage analysis result.
struct StoreAnalysis {
    // Packed segments
    segment_count: usize,
    segment_errors: usize,
    segment_disk_bytes: u64,
    segment_entries: u64,
    segment_logical_bytes: u64,
    segment_stored_bytes: u64,
    by_kind: BTreeMap<u16, KindStats>,
    by_codec: BTreeMap<u8, CodecStats>,
    content_in_segments_count: u64,
    content_in_segments_logical: u64,
    content_in_segments_stored: u64,

    // SQLite locator
    sqlite_bytes: u64,

    // Loose objects (metadata > threshold)
    loose_object_count: u64,
    loose_object_disk_bytes: u64,
    loose_object_logical_bytes: u64,
    loose_by_kind: BTreeMap<u16, LooseKindStats>,
    loose_object_errors: u64,

    // Loose content (file blobs > threshold)
    loose_content_count: u64,
    loose_content_disk_bytes: u64,
}

fn analyze_segments(storage_root: &Path) -> Result<StoreAnalysis> {
    let segments_dir = storage_root.join("packed/segments");
    let mut analysis = StoreAnalysis {
        segment_count: 0,
        segment_errors: 0,
        segment_disk_bytes: 0,
        segment_entries: 0,
        segment_logical_bytes: 0,
        segment_stored_bytes: 0,
        by_kind: BTreeMap::new(),
        by_codec: BTreeMap::new(),
        content_in_segments_count: 0,
        content_in_segments_logical: 0,
        content_in_segments_stored: 0,
        sqlite_bytes: 0,
        loose_object_count: 0,
        loose_object_disk_bytes: 0,
        loose_object_logical_bytes: 0,
        loose_by_kind: BTreeMap::new(),
        loose_object_errors: 0,
        loose_content_count: 0,
        loose_content_disk_bytes: 0,
    };

    // --- Packed segments ---
    if segments_dir.is_dir() {
        let mut segment_files: Vec<_> = fs::read_dir(&segments_dir)
            .context("read segments directory")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "segment"))
            .collect();
        segment_files.sort();

        for path in &segment_files {
            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    analysis.segment_errors += 1;
                    continue;
                }
            };
            let reader = match packed_segment_reader::open_segment(file) {
                Ok(r) => r,
                Err(_) => {
                    analysis.segment_errors += 1;
                    continue;
                }
            };

            let file_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            analysis.segment_disk_bytes += file_len;
            analysis.segment_count += 1;

            // Track unique blocks per segment to avoid double-counting shared
            // stored_length for codec=4 (ZstdBlock) entries.
            let mut seen_blocks: HashSet<(u64, u64)> = HashSet::new();

            for entry in reader.entries() {
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
        }
    }

    // --- SQLite locator ---
    let sqlite_path = storage_root.join("packed/catalog-locator.sqlite3");
    if sqlite_path.is_file() {
        analysis.sqlite_bytes = fs::metadata(&sqlite_path).map(|m| m.len()).unwrap_or(0);
    }

    // --- Loose objects (metadata) ---
    let loose_objects_dir = storage_root.join("loose-objects");
    if loose_objects_dir.is_dir() {
        walk_loose_objects(&loose_objects_dir, &mut analysis)?;
    }

    // --- Loose content (file blobs) ---
    let loose_content_dir = storage_root.join("loose-content");
    if loose_content_dir.is_dir() {
        walk_loose_content(&loose_content_dir, &mut analysis)?;
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
            let disk_bytes = entry.metadata()?.len();
            analysis.loose_object_count += 1;
            analysis.loose_object_disk_bytes += disk_bytes;

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

fn walk_loose_content(root: &Path, analysis: &mut StoreAnalysis) -> Result<()> {
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
            analysis.loose_content_count += 1;
            analysis.loose_content_disk_bytes += entry.metadata()?.len();
        }
    }
    Ok(())
}

/// Format the analysis as a human-readable report.
fn format_report(a: &StoreAnalysis) -> String {
    let mut out = String::new();

    let total_disk = a.segment_disk_bytes
        + a.sqlite_bytes
        + a.loose_object_disk_bytes
        + a.loose_content_disk_bytes;

    writeln!(out, "wheat store analysis").unwrap();
    writeln!(
        out,
        "═══════════════════════════════════════════════════════════════"
    )
    .unwrap();
    writeln!(out).unwrap();

    // Grand total
    writeln!(out, "total on disk:  {}", format_bytes(total_disk)).unwrap();
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
        "  {:<25} {:>8} {:>12} {:>12} {:>8}",
        "Kind", "Count", "Logical", "Stored", "Avg"
    )
    .unwrap();
    writeln!(
        out,
        "  {:<25} {:>8} {:>12} {:>12} {:>8}",
        "─────────────────────────", "────────", "────────────", "────────────", "────────"
    )
    .unwrap();
    let mut kinds: Vec<_> = a.by_kind.iter().collect();
    kinds.sort_by_key(|(_, v)| std::cmp::Reverse(v.logical_bytes));
    for (kind, stats) in &kinds {
        let avg = stats.logical_bytes.checked_div(stats.count).unwrap_or(0);
        writeln!(
            out,
            "  {:<25} {:>8} {:>12} {:>12} {:>8}",
            kind_name(**kind),
            stats.count,
            format_bytes(stats.logical_bytes),
            format_bytes(stats.stored_bytes),
            format_bytes(avg),
        )
        .unwrap();
    }
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

    let analysis = analyze_segments(&storage_root)?;
    let human = format_report(&analysis);

    let total_disk = analysis.segment_disk_bytes
        + analysis.sqlite_bytes
        + analysis.loose_object_disk_bytes
        + analysis.loose_content_disk_bytes;

    crate::command_output::Success::new(
        "store.analyze",
        serde_json::json!({
            "total_disk_bytes": total_disk,
            "segment_disk_bytes": analysis.segment_disk_bytes,
            "segment_count": analysis.segment_count,
            "segment_entries": analysis.segment_entries,
            "segment_logical_bytes": analysis.segment_logical_bytes,
            "loose_object_count": analysis.loose_object_count,
            "loose_object_disk_bytes": analysis.loose_object_disk_bytes,
            "loose_content_count": analysis.loose_content_count,
            "loose_content_disk_bytes": analysis.loose_content_disk_bytes,
            "sqlite_bytes": analysis.sqlite_bytes,
        }),
        human,
    )
}
