//! Restoring a served repository to a snapshot (`wheat_server::restore`), at two
//! repository sizes.
//!
//! A restore stages the snapshot's whole closure, walks it to check it is the
//! manifest's inventory, validates it whole, and names what the head relies on: a
//! fixed number of object reads per object in the snapshot. The case restores a
//! snapshot of one repository, and of one with [`GROWTH`] times its files, each into
//! an empty served repository, and counts the reads from its store per snapshot
//! object. Reads the stage answers from what it still holds are not counted, so only
//! the large repository, which is larger than the stage holds, is held to the budget:
//! at most [`READS_PER_OBJECT`] reads per object. A restore that re-read the stage
//! per batch, or walked the closure once per item, would read far more.

use std::{path::Path, sync::Arc, time::Instant};

use anyhow::{Result, ensure};
use wheat_server::{
    access::Principal, network::Network, registry::RepositoryEntry, restore::restore_from,
};
use wheat_store::{SeparateRepositoryStore, SnapshotLimits, export_snapshot};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch},
    mirror::{Shared, Unstamped},
    snapshot_export::build,
};

pub(crate) const NAME: &str = "server_restore";
/// The large repository has this many times the files of the small one.
const GROWTH: usize = 16;
/// Most reads per snapshot object restoring the large repository may take: one
/// walk for the inventory, one for the validation, one for the naming, and one to
/// spare. Measured 2026-11-16 (release): 2.05 for 101,244 objects (160,000 files);
/// 0.30 for 3,182, which the stage held nearly whole.
const READS_PER_OBJECT: f64 = 4.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(10_000, 100);
    let changes = context.scale.pick(50, 3);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("small_files", files);
    recorder.parameter("changes", changes);
    recorder.parameter("growth", GROWTH);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure(
        &context.scratch.join("small"),
        files,
        changes,
        "small",
        &mut recorder,
    ))?;
    runtime.block_on(measure(
        &context.scratch.join("large"),
        files * GROWTH,
        changes,
        "large",
        &mut recorder,
    ))?;
    recorder.at_most("large.reads_per_object", READS_PER_OBJECT, "count");
    Ok(recorder.finish())
}

/// Builds a repository of `files` files, snapshots it, and restores the snapshot
/// into an empty served repository, recording the reads that took per snapshot
/// object.
async fn measure(
    root: &Path,
    files: usize,
    changes: usize,
    step: &str,
    recorder: &mut Recorder,
) -> Result<()> {
    let source = Scratch::open(&root.join("source"))?;
    build(&source, files, changes).await?;
    let snapshot = export_snapshot(
        &source.objects,
        &source.content,
        &source.heads,
        SnapshotLimits::default(),
    )
    .await?;
    let objects = snapshot.manifest.objects.len();
    recorder.value(&format!("{step}.objects"), objects as f64, "count");
    let object_bytes: u64 = snapshot.manifest.objects.iter().map(|e| e.bytes).sum();
    recorder.value(
        &format!("{step}.object_bytes"),
        object_bytes as f64,
        "bytes",
    );
    recorder.value(
        &format!("{step}.contents"),
        snapshot.manifest.contents.len() as f64,
        "count",
    );

    let target = Scratch::open(&root.join("target"))?;
    target.initialize().await?;
    let counting = Arc::new(Counting::new(target.objects.clone()));
    let entry = Arc::new(RepositoryEntry::in_network(
        Network::with_naming(
            Box::new(SeparateRepositoryStore::new(
                Shared(Arc::clone(&counting)),
                target.content.clone(),
            )),
            Arc::new(Unstamped),
        ),
        target.heads.clone(),
    ));
    let principal = Principal {
        name: "wheat-bench".into(),
    };
    let envelope = snapshot.manifest.envelope;

    counting.take();
    let started = Instant::now();
    let restored = restore_from(
        &entry,
        &principal,
        Arc::new(snapshot),
        SnapshotLimits::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("restore: {e:?}"))?;
    recorder.wall(&format!("{step}.restore"), started.elapsed().as_secs_f64());
    let (reads, _) = counting.take();
    ensure!(restored.moved, "the restore did not move the head");
    ensure!(
        restored.head.envelope == envelope,
        "the head is not at the snapshot"
    );
    recorder.value(&format!("{step}.reads"), reads as f64, "count");
    recorder.value(
        &format!("{step}.reads_per_object"),
        reads as f64 / objects as f64,
        "count",
    );
    std::fs::remove_dir_all(root)?;
    Ok(())
}
