//! Validating a whole repository with no trusted state before it, as a clone does,
//! two ways: full validation (`validate_envelope`, with its scratch index) and the
//! incremental validator run from a state holding only the base view
//! (`validate_bootstrap`), where every retained object is new.
//!
//! Both prove the same invariants. The case builds one history of changes created on
//! the frontier, each adding one file two directories deep, and at each checkpoint
//! records each validator's wall time, objects and bytes read, and peak resident memory,
//! to choose which a clone uses.
//!
//! The same is recorded for a history whose files all sit in one directory (`flat.`
//! steps): every change makes a new version of one wide directory, stored in chunks,
//! and most of its chunks are unchanged from the version before.

use std::{fs, path::Path, time::Instant};

use anyhow::Result;
use wheat_store::{LocalPublicationStore, PersistedRepositoryMutator, RepositoryReader};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, create_at},
};
use crate::measure::PhasePeak;

pub(crate) const NAME: &str = "bootstrap_validate";

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 10_000, 30_000], vec![20, 60]);
    let flat = context.scale.pick(vec![1_000, 10_000], vec![20, 60]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("flat_checkpoints", &flat);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_history(
        &context.scratch.join("spread"),
        Layout::Spread,
        "",
        &checkpoints,
        &mut recorder,
    ))?;
    runtime.block_on(measure_history(
        &context.scratch.join("flat"),
        Layout::Flat,
        "flat.",
        &flat,
        &mut recorder,
    ))?;
    Ok(recorder.finish())
}

async fn measure_history(
    root: &Path,
    layout: Layout,
    prefix: &str,
    checkpoints: &[usize],
    recorder: &mut Recorder,
) -> Result<()> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let reader = RepositoryReader::new(&objects, &scratch.content);
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut effective = base;
    let started = Instant::now();
    for index in 0..largest {
        (head, effective) = create_at(
            &mutator,
            &scratch.content,
            head,
            base,
            effective,
            index,
            layout,
        )
        .await?;
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let step = format!("{prefix}n{size}");

        objects.take();
        let mut peak = PhasePeak::start()?;
        let full = Instant::now();
        reader.validate_envelope(head.envelope).await?;
        recorder.wall(&format!("{step}.full"), full.elapsed().as_secs_f64());
        recorder.value(&format!("{step}.full.peak_rss"), peak.take()?, "bytes");
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.full.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.full.bytes"), bytes as f64, "bytes");

        let bootstrap = Instant::now();
        reader.validate_bootstrap(head.envelope).await?;
        recorder.wall(
            &format!("{step}.bootstrap"),
            bootstrap.elapsed().as_secs_f64(),
        );
        recorder.value(&format!("{step}.bootstrap.peak_rss"), peak.take()?, "bytes");
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.bootstrap.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.bootstrap.bytes"), bytes as f64, "bytes");
        eprintln!(
            "{NAME}: {prefix}{size} changes after {:.0} s",
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
