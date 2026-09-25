//! Durable small appends to the packed store's active segment (formerly the ignored
//! `packed_active_bench::durable_packed_active_small_append_scaling` test).
//!
//! Every `put_if_absent` makes the active segment durable, so this is the deliberately
//! expensive one-object path: bulk ingestion must batch instead. Measured only; the
//! qualification host sustained roughly 39–49 objects/second.

use std::time::Instant;

use anyhow::Result;
use wheat_core::{ContentId, DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId};
use wheat_store::{ImmutableObjectStore, PackedObjectStore};

use super::{CaseContext, CaseOutput, Recorder};

pub(crate) const NAME: &str = "packed_append";

struct Fixture {
    id: ObjectId,
    canonical_bytes: Vec<u8>,
}

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let counts: &[usize] = context.scale.pick(&[128, 512, 2_048], &[8, 32]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("counts", counts);

    // Encoding and hashing happen before anything is timed.
    let fixtures = fixtures(counts.iter().copied().max().unwrap_or(0))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    for &count in counts {
        let fixtures = &fixtures[..count];
        let root = tempfile::tempdir_in(&context.scratch)?;
        let store = PackedObjectStore::open(root.path())?;
        let started = Instant::now();
        runtime.block_on(async {
            for fixture in fixtures {
                store
                    .put_if_absent(fixture.id, &fixture.canonical_bytes)
                    .await?;
            }
            anyhow::Ok(())
        })?;
        let wall = started.elapsed().as_secs_f64();
        // Each append leaves the active segment complete and durable, so its size now
        // is the final active-segment size.
        let segment = std::fs::metadata(root.path().join("packed/active.segment"))?.len();
        store.close()?;

        let step = format!("n{count}");
        recorder.wall(&step, wall);
        recorder.value(
            &format!("{step}.objects_per_second"),
            count as f64 / wall,
            "1/s",
        );
        recorder.value(
            &format!("{step}.canonical_bytes"),
            fixtures
                .iter()
                .map(|fixture| fixture.canonical_bytes.len())
                .sum::<usize>() as f64,
            "bytes",
        );
        recorder.value(
            &format!("{step}.active_segment_bytes"),
            segment as f64,
            "bytes",
        );
    }
    Ok(recorder.finish())
}

/// One-entry directory nodes: valid canonical metadata objects, each unique.
fn fixtures(count: usize) -> Result<Vec<Fixture>> {
    (0..count)
        .map(|index| {
            let canonical_bytes = DirectoryNode::new([(
                format!("entry-{index:08x}"),
                DirectoryEntry::File(FileValue {
                    content: ContentId::of(&(index as u64).to_be_bytes()),
                    kind: FileKind::Regular,
                }),
            )])?
            .canonical_bytes();
            Ok(Fixture {
                id: ObjectId::of_canonical_bytes(&canonical_bytes),
                canonical_bytes,
            })
        })
        .collect()
}
