//! Transition validation performance benchmark.
//!
//! Compares `validate_transition` (O(delta)) against `validate_complete`
//! (O(repository)) to demonstrate that the bounded delta approach scales
//! with the change size, not the repository size.
//!
//! Run with:
//! ```
//! cargo test -p wheat-store --test transition_bench --release -- --ignored --nocapture
//! ```
//!
//! Override defaults with environment variables:
//! - `WHEAT_BENCH_BASELINE_CHANGES` — number of changes in the baseline (default 500)
//! - `WHEAT_BENCH_DELTA_CHANGES` — number of changes in the delta (default 5)
//! - `WHEAT_BENCH_SAMPLES` — repetitions per measurement (default 3)

use std::time::Instant;

use wheat_core::{
    ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    Repository, RepositoryStateId, RevisionEdits, Tree,
};
use wheat_store::{
    ContentStore, LocalPublicationStore, LooseObjectStore, RepositoryPublisher, RepositoryReader,
};

fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

fn stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Build a repository with `baseline_count` changes, publish, add
/// `delta_count` more changes, publish again. Returns the two state IDs.
async fn build_repository(
    objects: &LooseObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    baseline_count: usize,
    delta_count: usize,
) -> (RepositoryStateId, RepositoryStateId) {
    // Pre-populate content store with all values we'll reference.
    for i in 0..(baseline_count + delta_count) {
        content.put(format!("value-{i}").as_bytes()).unwrap();
    }

    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    let mut effective = basis;

    // Build baseline: `baseline_count` sequential changes.
    for i in 0..baseline_count {
        let (_, outcome) = repository
            .create_change(
                basis,
                effective,
                [],
                edit(
                    &format!("src/baseline/{i:06}.txt"),
                    format!("value-{i}").as_bytes(),
                ),
                ApplicationContext::new(content),
            )
            .unwrap();
        effective = outcome.resulting_view;
    }

    let publisher = RepositoryPublisher::new(objects, content, heads);
    let baseline_head = publisher.initialize(&repository).await.unwrap();
    let baseline_envelope = RepositoryReader::new(objects, content)
        .envelope(baseline_head.envelope)
        .await
        .unwrap();
    let baseline_state = baseline_envelope.state;

    // Build delta: `delta_count` additional changes on top.
    for i in 0..delta_count {
        let j = baseline_count + i;
        let (_, outcome) = repository
            .create_change(
                basis,
                effective,
                [],
                edit(
                    &format!("src/delta/{i:06}.txt"),
                    format!("value-{j}").as_bytes(),
                ),
                ApplicationContext::new(content),
            )
            .unwrap();
        effective = outcome.resulting_view;
    }

    let delta_head = publisher.publish(baseline_head, &repository).await.unwrap();
    let delta_envelope = RepositoryReader::new(objects, content)
        .envelope(delta_head.envelope)
        .await
        .unwrap();
    let delta_state = delta_envelope.state;

    (baseline_state, delta_state)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
async fn validate_transition_vs_validate_complete() {
    let baseline_count = env_usize("WHEAT_BENCH_BASELINE_CHANGES", 500);
    let delta_count = env_usize("WHEAT_BENCH_DELTA_CHANGES", 5);
    let samples = env_usize("WHEAT_BENCH_SAMPLES", 3).max(1);

    println!(
        "Building repository: {baseline_count} baseline changes + {delta_count} delta changes"
    );
    let dir = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(dir.path());

    let started_build = Instant::now();
    let (baseline_state, delta_state) =
        build_repository(&objects, &content, &heads, baseline_count, delta_count).await;
    let build_elapsed = started_build.elapsed();
    println!("Repository built in {:.3}s", build_elapsed.as_secs_f64());

    let reader = RepositoryReader::new(&objects, &content);

    // Warm up: one untimed run of each to populate OS caches.
    reader
        .validate_transition(baseline_state, delta_state)
        .await
        .unwrap();
    reader.validate_complete(delta_state).await.unwrap();

    // Measure validate_transition (O(delta)).
    let mut transition_times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        reader
            .validate_transition(baseline_state, delta_state)
            .await
            .unwrap();
        transition_times.push(started.elapsed());
    }

    // Measure validate_complete (O(repository)).
    let mut complete_times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        reader.validate_complete(delta_state).await.unwrap();
        complete_times.push(started.elapsed());
    }

    // Report.
    let transition_median = {
        let mut sorted: Vec<f64> = transition_times.iter().map(|d| d.as_secs_f64()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[sorted.len() / 2]
    };
    let complete_median = {
        let mut sorted: Vec<f64> = complete_times.iter().map(|d| d.as_secs_f64()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[sorted.len() / 2]
    };
    let speedup = complete_median / transition_median;

    println!();
    println!("=== Transition Validation Benchmark ===");
    println!(
        "Baseline: {baseline_count} changes, Delta: {delta_count} changes, Samples: {samples}"
    );
    println!();
    println!(
        "validate_transition (O(delta)):      median {:.6}s  (all: {})",
        transition_median,
        transition_times
            .iter()
            .map(|d| format!("{:.6}s", d.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "validate_complete   (O(repository)): median {:.6}s  (all: {})",
        complete_median,
        complete_times
            .iter()
            .map(|d| format!("{:.6}s", d.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    println!("Speedup: {speedup:.1}x");
    println!();

    // Performance assertion: validate_transition should complete in under 1s
    // for a delta of ~5 changes on a baseline of 500+ changes. This is the
    // budget from the design doc and Rule #20.
    assert!(
        transition_median < 1.0,
        "validate_transition median ({transition_median:.3}s) exceeded the 1s budget"
    );

    // The delta approach should be meaningfully faster than the full approach.
    // With 500 baseline + 5 delta, we expect at least a 5x speedup. Being
    // conservative: assert at least 2x to avoid flaky failures on slow CI.
    assert!(
        speedup > 2.0,
        "validate_transition should be at least 2x faster than validate_complete, \
         but speedup was only {speedup:.1}x"
    );
}
