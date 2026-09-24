//! Opt-in memory profile of `wheat repack`: peak RSS of each phase, measured
//! by resetting the kernel's high-water mark between phases.
//!
//! It **mutates** the store it is given (it runs a real repack), so point it at
//! a disposable copy only:
//! `WHEAT_REPACK_PROFILE_STORE=/tmp/copy/store cargo test --release -p wheat-store \
//!   --test repack_memory_profile -- --ignored --nocapture`
//!
//! Set `WHEAT_REPACK_PROFILE_MAX_CONTENT_IDS` to profile a repository with more
//! content IDs than the default repack bound.

use std::path::PathBuf;
use std::time::Instant;

use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PublicationStore, RepackOptions, RepositoryReader,
};

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

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs WHEAT_REPACK_PROFILE_STORE pointing at a disposable store copy; mutates it"]
async fn repack_phase_memory() {
    let root = PathBuf::from(
        std::env::var("WHEAT_REPACK_PROFILE_STORE").expect("WHEAT_REPACK_PROFILE_STORE"),
    );
    reset_peak();
    let started = Instant::now();
    let objects = PackedObjectStore::open(root.join("storage")).unwrap();
    let heads = LocalPublicationStore::new(root.join("repository.head"));
    report("open", started);

    let started = Instant::now();
    let content = objects.content_store();
    let reader = RepositoryReader::new(&objects, &content);
    reader
        .validate_envelope(heads.read().unwrap().envelope)
        .await
        .unwrap();
    report("validate envelope", started);

    // Planning validates the envelope again, then plans from stored records.
    // `WHEAT_REPACK_PROFILE_MAX_CONTENT_IDS` lifts the content-ID bound so
    // repositories above the default can be measured.
    let mut options = RepackOptions::default();
    if let Ok(limit) = std::env::var("WHEAT_REPACK_PROFILE_MAX_CONTENT_IDS") {
        options.content_limits.max_content_ids = limit.parse().unwrap();
    }
    let started = Instant::now();
    let plan = objects.plan_repack(&heads, options).await.unwrap();
    report("plan repack", started);

    let started = Instant::now();
    let result = objects.execute_repack(&heads, plan).unwrap();
    report("execute repack", started);
    eprintln!("  no-op {}", result.no_op);
}
