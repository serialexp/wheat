//! Durable active-segment small-append scaling benchmark.
//!
//! This is intentionally ignored because every operation synchronizes durable
//! packed-store state. Run it with:
//! `cargo test -p wheat-store --test packed_active_bench --release -- --ignored --nocapture`
//!
//! Override the default count list with `WHEAT_PACKED_ACTIVE_BENCH_COUNTS`
//! (a comma-separated list, for example `128,512,2048`) and repeat each count
//! with `WHEAT_PACKED_ACTIVE_BENCH_SAMPLES`.

use std::{io, time::Instant};

use wheat_core::{ContentId, DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId};
use wheat_store::{ImmutableObjectStore, PackedObjectStore};

const DEFAULT_COUNTS: &[usize] = &[128, 512, 2_048];
const DEFAULT_SAMPLES: usize = 1;

struct Fixture {
    id: ObjectId,
    canonical_bytes: Vec<u8>,
}

fn benchmark_counts() -> Result<Vec<usize>, io::Error> {
    let Some(value) = std::env::var_os("WHEAT_PACKED_ACTIVE_BENCH_COUNTS") else {
        return Ok(DEFAULT_COUNTS.to_vec());
    };
    let value = value.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "WHEAT_PACKED_ACTIVE_BENCH_COUNTS must be valid Unicode",
        )
    })?;
    let counts = value
        .split(',')
        .map(|count| {
            count.trim().parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid WHEAT_PACKED_ACTIVE_BENCH_COUNTS value {count:?}: {error}"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if counts.is_empty() || counts.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WHEAT_PACKED_ACTIVE_BENCH_COUNTS must contain one or more positive counts",
        ));
    }
    Ok(counts)
}

fn benchmark_samples() -> Result<usize, io::Error> {
    match std::env::var("WHEAT_PACKED_ACTIVE_BENCH_SAMPLES") {
        Ok(value) => value.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid WHEAT_PACKED_ACTIVE_BENCH_SAMPLES value {value:?}: {error}"),
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SAMPLES),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WHEAT_PACKED_ACTIVE_BENCH_SAMPLES must be valid Unicode",
        )),
    }
    .and_then(|samples| {
        (samples > 0).then_some(samples).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "WHEAT_PACKED_ACTIVE_BENCH_SAMPLES must be positive",
            )
        })
    })
}

fn fixtures(count: usize) -> Vec<Fixture> {
    let mut fixtures = Vec::with_capacity(count);
    for index in 0..count {
        // One-entry directory nodes are valid canonical metadata objects. Both
        // the name and content identity make every fixture deterministic and unique.
        let canonical_bytes = DirectoryNode::new([(
            format!("entry-{index:08x}"),
            DirectoryEntry::File(FileValue {
                content: ContentId::of(&(index as u64).to_be_bytes()),
                kind: FileKind::Regular,
            }),
        )])
        .unwrap()
        .canonical_bytes();
        fixtures.push(Fixture {
            id: ObjectId::of_canonical_bytes(&canonical_bytes),
            canonical_bytes,
        });
    }
    fixtures
}

/// Measures one durable `put_if_absent` per small canonical metadata object.
///
/// Run with `cargo test -p wheat-store --test packed_active_bench --release -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "durable benchmark; run explicitly with --ignored --nocapture"]
async fn durable_packed_active_small_append_scaling() -> Result<(), Box<dyn std::error::Error>> {
    let counts = benchmark_counts()?;
    let samples = benchmark_samples()?;
    let max_count = *counts.iter().max().expect("validated non-empty counts");
    // Fixture creation, encoding, and hashing happen before every timed sample.
    let fixtures = fixtures(max_count);

    for count in counts {
        let fixtures = &fixtures[..count];
        let canonical_bytes = fixtures
            .iter()
            .map(|fixture| fixture.canonical_bytes.len())
            .sum::<usize>();
        for sample in 0..samples {
            let root = tempfile::tempdir()?;
            let store = PackedObjectStore::open(root.path())?;

            let started = Instant::now();
            for fixture in fixtures {
                store
                    .put_if_absent(fixture.id, &fixture.canonical_bytes)
                    .await?;
            }
            let elapsed = started.elapsed();
            // The active segment is fully authenticated and durable after each
            // append, so its live size is the final packed active-segment size.
            let active_segment_bytes =
                std::fs::metadata(root.path().join("packed/active.segment"))?.len();
            store.close()?;

            let wall_seconds = elapsed.as_secs_f64();
            println!(
                concat!(
                    "{{\"benchmark\":\"packed_active_small_append\",",
                    "\"count\":{},\"sample\":{},\"wall_seconds\":{:.6},",
                    "\"objects_per_second\":{:.3},\"canonical_bytes\":{},",
                    "\"active_segment_bytes\":{}}}"
                ),
                count,
                sample + 1,
                wall_seconds,
                count as f64 / wall_seconds,
                canonical_bytes,
                active_segment_bytes,
            );
        }
    }
    Ok(())
}
