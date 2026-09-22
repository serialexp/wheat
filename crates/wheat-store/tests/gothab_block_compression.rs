//! Measures block compression savings on the real wheat-gothab-atomic repository.
//!
//! Run: cargo test -p wheat-store --test gothab_block_compression -- --nocapture --ignored

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

// Fresh import with all phases (0, 1, 1.5, 2) active.
const WHEAT_REPO: &str =
    "/home/bart/.local/share/wheat/stores/01a0c44e-e3d2-7862-96f7-5265cce52099/storage/packed";

fn kind_name(kind: u16) -> &'static str {
    match kind {
        0 => "Content",
        1 => "Tree",
        2 => "Change",
        3 => "Revision",
        4 => "Application",
        5 => "Conflict",
        6 => "View",
        7 => "ContentManifest",
        8 => "RepositoryState",
        9 => "RevisionMapNode",
        10 => "ApplicationMapNode",
        11 => "ConflictSetNode",
        12 => "ContentSetNode",
        13 => "RetainedSetNode",
        14 => "ViewLabelManifest",
        15 => "Envelope",
        16 => "Edit",
        17 => "ChangeMapNode",
        18 => "ViewLineageMapNode",
        19 => "ContentSetSnapshot",
        20 => "ContentSetDelta",
        _ => "Unknown",
    }
}

#[test]
#[ignore = "requires wheat-gothab-atomic on disk; run with --nocapture --ignored"]
fn measure_gothab_block_compression() {
    let segments_dir = PathBuf::from(WHEAT_REPO).join("segments");
    if !segments_dir.exists() {
        eprintln!("Skipping: {segments_dir:?} does not exist");
        return;
    }

    let mut segment_files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&segments_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "segment") {
            continue;
        }
        segment_files.push(path);
    }
    segment_files.sort();

    println!();
    println!("============================================================");
    println!("  Gothab wheat repository — block compression analysis");
    println!("  Segments: {}", segment_files.len());
    println!("============================================================");

    // Tally: kind -> (count, logical_bytes, stored_bytes)
    let mut by_kind: BTreeMap<u16, (u64, u64, u64)> = BTreeMap::new();
    // Tally: codec -> (count, stored_bytes)
    let mut by_codec: BTreeMap<u8, (u64, u64)> = BTreeMap::new();
    let mut total_entries = 0u64;
    let mut total_logical = 0u64;
    let mut total_stored = 0u64;
    let mut total_segment_bytes = 0u64;

    // Objects eligible for block compression: metadata, logical < 1024
    let mut eligible_count = 0u64;
    let mut eligible_logical_bytes = 0u64;
    let mut _eligible_stored_bytes = 0u64;

    let mut segments_read = 0usize;
    let mut segments_failed = 0usize;

    for path in &segment_files {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => {
                segments_failed += 1;
                continue;
            }
        };
        let reader = match wheat_store::packed_segment_reader::open_segment(file) {
            Ok(r) => r,
            Err(e) => {
                if segments_failed == 0 {
                    eprintln!("  First segment error: {e}");
                }
                segments_failed += 1;
                continue;
            }
        };

        let file_len = std::fs::metadata(path).unwrap().len();
        total_segment_bytes += file_len;
        segments_read += 1;

        // Track unique blocks to avoid double-counting shared stored_length.
        let mut seen_blocks: std::collections::HashSet<(u64, u64)> =
            std::collections::HashSet::new();

        for entry in reader.entries() {
            total_entries += 1;
            total_logical += entry.logical_length;

            // For block entries, stored_length is the shared block size.
            // Only count it once per unique (offset, stored_length) block.
            let is_block = entry.codec == 4;
            if is_block {
                let bk = (entry.offset, entry.stored_length);
                if seen_blocks.insert(bk) {
                    total_stored += entry.stored_length;
                }
            } else {
                total_stored += entry.stored_length;
            }

            let kind_entry = by_kind.entry(entry.kind).or_insert((0, 0, 0));
            kind_entry.0 += 1;
            kind_entry.1 += entry.logical_length;
            // For per-kind, use logical_length for block entries to show
            // actual data size, not the shared block size
            kind_entry.2 += if is_block {
                entry.logical_length
            } else {
                entry.stored_length
            };

            let codec_entry = by_codec.entry(entry.codec).or_insert((0, 0));
            codec_entry.0 += 1;
            codec_entry.1 += if is_block {
                entry.logical_length
            } else {
                entry.stored_length
            };

            // Metadata namespace (1) with logical < 1024 = block-eligible
            if entry.namespace == 1 && entry.logical_length < 1024 {
                eligible_count += 1;
                eligible_logical_bytes += entry.logical_length;
                _eligible_stored_bytes += entry.logical_length; // logical, since stored is shared
            }
        }
    }

    println!();
    println!("  Segments read: {segments_read}, failed: {segments_failed}");
    println!("  Total entries: {total_entries}");
    println!(
        "  Total logical bytes: {total_logical} ({:.1} MiB)",
        total_logical as f64 / 1048576.0
    );
    println!(
        "  Total stored bytes:  {total_stored} ({:.1} MiB)",
        total_stored as f64 / 1048576.0
    );
    println!(
        "  Total segment files: {total_segment_bytes} ({:.1} MiB)",
        total_segment_bytes as f64 / 1048576.0
    );

    println!();
    println!("  By codec:");
    println!("  {:<12} {:>10} {:>12}", "Codec", "Count", "Stored (MiB)");
    for (codec, (count, stored)) in &by_codec {
        let name = match codec {
            0 => "Raw",
            1 => "Zstd",
            4 => "ZstdBlock",
            _ => "?",
        };
        println!(
            "  {:<12} {:>10} {:>12.1}",
            name,
            count,
            *stored as f64 / 1048576.0
        );
    }

    println!();
    println!("  By kind (sorted by logical bytes):");
    println!(
        "  {:<25} {:>8} {:>12} {:>12} {:>6}",
        "Kind", "Count", "Logical(MiB)", "Stored(MiB)", "Avg"
    );
    println!(
        "  {:<25} {:>8} {:>12} {:>12} {:>6}",
        "-------------------------", "--------", "------------", "------------", "------"
    );
    let mut kinds: Vec<_> = by_kind.iter().collect();
    kinds.sort_by_key(|(_k, v)| std::cmp::Reverse(v.1));
    for (kind, (count, logical, stored)) in &kinds {
        println!(
            "  {:<25} {:>8} {:>12.1} {:>12.1} {:>6}",
            kind_name(**kind),
            count,
            *logical as f64 / 1048576.0,
            *stored as f64 / 1048576.0,
            if *count > 0 { logical / count } else { 0 }
        );
    }

    println!();
    println!("  Block-compressed records:");
    println!("    Count:  {eligible_count}");
    println!(
        "    Logical bytes: {eligible_logical_bytes} ({:.1} MiB)",
        eligible_logical_bytes as f64 / 1048576.0
    );
    println!(
        "    Share of entries: {:.1}%",
        eligible_count as f64 / total_entries as f64 * 100.0
    );
    println!(
        "    Share of logical bytes: {:.1}%",
        eligible_logical_bytes as f64 / total_logical as f64 * 100.0
    );

    // The key metric: segment files vs logical bytes (compression ratio).
    println!();
    println!("  Storage efficiency:");
    println!(
        "    Logical bytes (sum of all records): {:.1} MiB",
        total_logical as f64 / 1048576.0
    );
    println!(
        "    Segment files on disk:              {:.1} MiB",
        total_segment_bytes as f64 / 1048576.0
    );
    println!("    Loose content on disk:              91.0 MiB");
    println!("    Loose objects on disk:               137.0 MiB");
    let total_on_disk = total_segment_bytes + 91 * 1048576 + 137 * 1048576;
    println!(
        "    Total on disk:                      {:.1} MiB",
        total_on_disk as f64 / 1048576.0
    );
    if total_logical > 0 {
        println!(
            "    Segment compression ratio:          {:.2}x",
            total_logical as f64 / total_segment_bytes as f64
        );
    }
    println!();
}
