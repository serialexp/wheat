//! Storing publications in a fork network, whose content is kept apart from its
//! objects.
//!
//! A repositories directory keeps each network's objects in the network's packed
//! store and every network's content in one shared packed store
//! (`wheat_store::NetworkStore`), so the write a group commit makes for a batch of
//! publications is two durable appends, one per store, where a repository with a
//! store of its own makes one. The case stores [`WRITES`] batches of [`OBJECTS`]
//! objects and [`BLOBS`] blobs both ways and records the median wall time of a write
//! and the durable appends each made. A network's write must make one append per
//! store, and may take at most [`SPLIT_BUDGET_PERCENT`] of the single store's time.

use std::time::Instant;

use anyhow::Result;
use wheat_core::{
    ContentId, DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId, VerifiedContent,
};
use wheat_store::{
    ContentItem, NetworkStore, PackedObjectStore, PackedStoreTrace, RepositoryStore,
};

use super::{CaseContext, CaseOutput, Recorder};
use crate::measure;

pub(crate) const NAME: &str = "network_publish";
/// Objects in each write: the directory nodes and commit objects of a publish.
const OBJECTS: usize = 32;
/// Blobs in each write.
const BLOBS: usize = 8;
const BLOB_BYTES: usize = 4096;
/// Two appends one after another, each with its own syncs, as Bart chose on
/// 2026-10-27 over running them at once (measured at 170%: they sync to one device).
/// Measured on 2026-10-27 (release, NVMe): 202–204%, 3.0 ms against 6.0 ms a write.
const SPLIT_BUDGET_PERCENT: f64 = 210.0;

/// One write's objects and blobs, each unique to it.
struct Batch {
    objects: Vec<(ObjectId, Vec<u8>)>,
    blobs: Vec<Vec<u8>>,
}

impl Batch {
    fn new(write: usize) -> Result<Self> {
        let objects = (0..OBJECTS)
            .map(|index| {
                let bytes = DirectoryNode::new([(
                    format!("w{write:06}-o{index:04}"),
                    DirectoryEntry::File(FileValue {
                        content: ContentId::of(format!("{write}/{index}").as_bytes()),
                        kind: FileKind::Regular,
                    }),
                )])?
                .canonical_bytes();
                Ok((ObjectId::of_canonical_bytes(&bytes), bytes))
            })
            .collect::<Result<_>>()?;
        let blobs = (0..BLOBS)
            .map(|index| {
                let mut blob = format!("write {write} blob {index}\n").into_bytes();
                blob.resize(BLOB_BYTES, b'.');
                blob
            })
            .collect();
        Ok(Self { objects, blobs })
    }

    fn content(&self) -> Vec<ContentItem<'_>> {
        self.blobs
            .iter()
            .map(|blob| ContentItem::new(VerifiedContent::new(blob.as_slice()), None))
            .collect()
    }
}

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let writes = context.scale.pick(200, 5);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("writes", writes);
    recorder.parameter("objects", OBJECTS);
    recorder.parameter("blobs", BLOBS);
    recorder.parameter("blob_bytes", BLOB_BYTES);

    // Encoding and hashing happen before anything is timed.
    let batches = (0..writes).map(Batch::new).collect::<Result<Vec<_>>>()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let scratch = tempfile::tempdir_in(&context.scratch)?;

    let (store, trace) = PackedObjectStore::open_traced(scratch.path().join("single"))?;
    let single = runtime.block_on(measure_writes(&store, &batches, &[&trace]))?;
    store.close()?;

    let (pool, pool_trace) = PackedObjectStore::open_traced(scratch.path().join("pool"))?;
    let (content, content_trace) = PackedObjectStore::open_traced(scratch.path().join("content"))?;
    let network = NetworkStore::new(pool, content);
    let split = runtime.block_on(measure_writes(
        &network,
        &batches,
        &[&pool_trace, &content_trace],
    ))?;

    recorder.wall("single", single.wall);
    recorder.value("single.appends_per_write", single.appends, "count");
    recorder.wall("network", split.wall);
    recorder.value("network.appends_per_write", split.appends, "count");
    recorder.value(
        "network.over_single",
        100.0 * split.wall / single.wall,
        "percent",
    );
    recorder.at_most("network.appends_per_write", 2.0, "count");
    recorder.at_most("network.over_single", SPLIT_BUDGET_PERCENT, "percent");
    Ok(recorder.finish())
}

struct Writes {
    /// The median wall time of a write.
    wall: f64,
    /// Durable appends per write, over every store traced.
    appends: f64,
}

async fn measure_writes(
    store: &impl RepositoryStore,
    batches: &[Batch],
    traces: &[&PackedStoreTrace],
) -> Result<Writes> {
    let appends = || {
        traces
            .iter()
            .map(|trace| trace.snapshot().map_or(0, |counts| counts.durable_appends))
            .sum::<u64>()
    };
    let before = appends();
    let mut walls = Vec::with_capacity(batches.len());
    for batch in batches {
        let content = batch.content();
        let started = Instant::now();
        store.put_publication(&batch.objects, &content).await?;
        walls.push(started.elapsed().as_secs_f64());
    }
    Ok(Writes {
        wall: measure::median(walls),
        appends: (appends() - before) as f64 / batches.len() as f64,
    })
}
