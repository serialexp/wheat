//! Opt-in scale profile of the content repack planner: wall time and peak RSS
//! of planning and preparing a generation with many content IDs.
//!
//! `WHEAT_CONTENT_SCALE_IDS=1000000 cargo test --release -p wheat-store \
//!   --test content_repack_scale -- --ignored --nocapture`

use std::time::Instant;

use wheat_core::ContentId;
use wheat_store::{
    ContentHistoryPair, ContentRepackInput, ContentRepackLimits, ContentRepackPlan, ContentStore,
    Durability, plan_content_repack, prepare_content_repack,
};

/// Versions per synthetic file: one tip plus this many minus one history pairs.
const VERSIONS_PER_FILE: usize = 20;

/// Current and peak RSS in MiB, from `/proc/self/status`.
fn rss_mib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| {
                value
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse::<u64>()
                    .unwrap()
            })
            .unwrap()
            / 1024
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// Resets the peak-RSS high-water mark (Linux `clear_refs` value 5).
fn reset_peak() {
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
}

fn report(phase: &str, started: Instant) {
    let (now, peak) = rss_mib();
    eprintln!(
        "{phase:<28} {:>7.1} s  peak {peak:>6} MiB  after {now:>6} MiB",
        started.elapsed().as_secs_f64()
    );
    reset_peak();
}

/// Version `version` of file `file`: a few hundred bytes that differ from the
/// neighbouring version in one line, so each history pair deltas well.
fn body(file: usize, version: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(512);
    for line in 0..16 {
        let stamp = if line == version % 16 { version } else { 0 };
        bytes.extend_from_slice(format!("file {file} line {line} stamp {stamp}\n").as_bytes());
    }
    bytes
}

#[test]
#[ignore = "creates WHEAT_CONTENT_SCALE_IDS loose content files; run in release"]
fn content_repack_planner_scale() {
    let ids: usize = std::env::var("WHEAT_CONTENT_SCALE_IDS")
        .expect("WHEAT_CONTENT_SCALE_IDS")
        .parse()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = ContentStore::with_durability(dir.path(), Durability::Deferred);

    let started = Instant::now();
    let mut content_ids = Vec::with_capacity(ids);
    let mut tips = Vec::new();
    let mut history = Vec::with_capacity(ids);
    let files = ids.div_ceil(VERSIONS_PER_FILE);
    for file in 0..files {
        let versions = VERSIONS_PER_FILE.min(ids - file * VERSIONS_PER_FILE);
        let mut batch = Vec::with_capacity(versions);
        for version in 0..versions {
            let bytes = body(file, version);
            batch.push((ContentId::of(&bytes), bytes));
        }
        for pair in batch.windows(2) {
            history.push(ContentHistoryPair {
                older: pair[0].0,
                newer: pair[1].0,
            });
        }
        tips.push(batch.last().unwrap().0);
        content_ids.extend(batch.iter().map(|(id, _)| *id));
        store.put_many(batch, 1).unwrap();
    }
    eprintln!(
        "{ids} content IDs, {} history pairs, {} tips",
        history.len(),
        tips.len()
    );
    report("write content", started);

    let limits = ContentRepackLimits {
        max_content_ids: usize::MAX,
        ..ContentRepackLimits::default()
    };
    let started = Instant::now();
    let plan = plan_content_repack(
        &store,
        None,
        ContentRepackInput {
            content_ids,
            tips,
            history,
        },
        limits,
    )
    .unwrap();
    report("plan", started);
    let ContentRepackPlan::Replace(analysis) = plan else {
        panic!("a fresh generation is never a no-op");
    };
    eprintln!("  estimated bytes {}", analysis.estimated_bytes());

    let started = Instant::now();
    let prepared = prepare_content_repack(&store, analysis).unwrap();
    report("prepare", started);
    eprintln!(
        "  entries {}  scratch bytes {}",
        prepared.manifest.entries.len(),
        prepared.scratch_bytes()
    );
}
