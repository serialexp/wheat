//! Opt-in, read-only random-read latency benchmark for in-segment metadata
//! delta chains: how much slower is a record at chain depth `d` than a whole
//! record of the same kind?
//!
//! It opens segment files directly (no catalog, no recovery), so it never
//! mutates the store. Run against a disposable repacked store:
//! `WHEAT_DELTA_LATENCY_STORE=/path/to/store cargo test --release -p wheat-store \
//!   --test metadata_delta_read_latency -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wheat_store::ObjectKind;
use wheat_store::packed_segment_reader::open_segment;

/// Records timed per (kind, depth) bucket; deterministic (digest order).
const SAMPLES: usize = 400;
const DEPTHS: &[u8] = &[0, 1, 4, 8, 12, 16];
const KINDS: &[ObjectKind] = &[
    ObjectKind::ContentMapNode,
    ObjectKind::Tree,
    ObjectKind::RevisionMapNode,
];

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() - 1) * p / 100]
}

#[test]
#[ignore = "opt-in benchmark against a disposable repacked store"]
fn measure_metadata_delta_read_latency() {
    let Some(store) = std::env::var_os("WHEAT_DELTA_LATENCY_STORE") else {
        eprintln!("set WHEAT_DELTA_LATENCY_STORE to a disposable repacked store");
        return;
    };
    let segments = PathBuf::from(store).join("storage/packed/segments");
    let mut paths = std::fs::read_dir(&segments)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "segment"))
        .collect::<Vec<_>>();
    paths.sort();

    for path in paths {
        let mut segment = open_segment(File::open(&path).unwrap()).unwrap();
        let mut buckets = BTreeMap::<(u16, u8), Vec<[u8; 32]>>::new();
        for entry in segment.entries() {
            if entry.namespace != 1 || !DEPTHS.contains(&entry.delta_depth) {
                continue;
            }
            let bucket = buckets.entry((entry.kind, entry.delta_depth)).or_default();
            if bucket.len() < SAMPLES {
                bucket.push(entry.digest);
            }
        }
        if buckets.is_empty() {
            continue;
        }
        println!("segment {}", path.display());
        println!(
            "  {:<20} {:>5} {:>7} {:>10} {:>10} {:>10} {:>10}",
            "kind", "depth", "reads", "p50", "p99", "max", "avg bytes"
        );
        for kind in KINDS {
            for depth in DEPTHS {
                let Some(digests) = buckets.get(&(*kind as u16, *depth)) else {
                    continue;
                };
                let mut times = Vec::with_capacity(digests.len());
                let mut bytes = 0usize;
                for digest in digests {
                    // Whole records in shared blocks must not hit a block
                    // decompressed by the previous read.
                    segment.clear_block_cache();
                    let started = Instant::now();
                    let payload = segment
                        .read_entry_bounded(1, *digest, usize::MAX)
                        .unwrap()
                        .expect("indexed record is readable");
                    times.push(started.elapsed());
                    bytes += payload.len();
                }
                times.sort_unstable();
                println!(
                    "  {:<20} {:>5} {:>7} {:>10.1?} {:>10.1?} {:>10.1?} {:>10}",
                    format!("{kind:?}"),
                    depth,
                    times.len(),
                    percentile(&times, 50),
                    percentile(&times, 99),
                    times.last().unwrap(),
                    bytes / times.len(),
                );
            }
        }
    }
}
