//! Loading the desktop client's Changes list as the history grows.
//!
//! Every load of the desktop's live backend reads the working view's changes with
//! `wheat_client::history::entries`: each change's current revision and its record.
//! Changes are created on the frontier and published one at a time, the way
//! `wheat change new` does. At each checkpoint the working view's history is read
//! once cold (nothing of it read yet in this process) and [`SAMPLES`] more times,
//! recording the objects and bytes a read takes and the median wall time.
//!
//! A load reads each change's revision and change records and walks the revision
//! map once; the change map's nodes on the way to each change stay cached, so a load
//! reads about four objects per change at any size. At [`LARGEST`] changes it may
//! read at most [`READS_PER_CHANGE`] objects and [`BYTES_PER_CHANGE`] bytes per change
//! (the map nodes it reads grow with the history), and finish within
//! [`LOAD_BUDGET_SECONDS`]. Measured 2026-10-22 (release): 4.01 reads, 11.9 KB and
//! 0.62 s at 20,000 changes; 4.07 reads and 7.1 KB per change at 1,000.

use std::{fs, path::Path, time::Instant};

use anyhow::Result;
use wheat_store::{LocalPublicationStore, PersistedRepositoryMutator};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, create_at},
};
use crate::measure;

pub(crate) const NAME: &str = "client_history";
/// Warm loads after the first, per checkpoint.
const SAMPLES: usize = 3;
/// The history size the budgets are checked at.
const LARGEST: usize = 20_000;
const LOAD_BUDGET_SECONDS: f64 = 1.0;
const READS_PER_CHANGE: f64 = 5.0;
const BYTES_PER_CHANGE: f64 = 16_384.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 5_000, LARGEST], vec![100, 300]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_history(
        &context.scratch.join("history"),
        &checkpoints,
        &mut recorder,
    ))?;

    let step = format!("n{LARGEST}.load");
    recorder.at_most(&format!("{step}.wall"), LOAD_BUDGET_SECONDS, "s");
    recorder.at_most(
        &format!("{step}.reads_per_change"),
        READS_PER_CHANGE,
        "count",
    );
    recorder.at_most(
        &format!("{step}.bytes_per_change"),
        BYTES_PER_CHANGE,
        "bytes",
    );
    Ok(recorder.finish())
}

async fn measure_history(
    root: &Path,
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
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut effective = base;
    for index in 0..largest {
        (head, effective) = create_at(
            &mutator,
            &scratch.content,
            head,
            base,
            effective,
            index,
            Layout::Spread,
        )
        .await?;
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let step = format!("n{size}");
        objects.take();
        let started = Instant::now();
        let entries =
            wheat_client::history::entries(&objects, &scratch.content, head, effective).await?;
        let cold = started.elapsed().as_secs_f64();
        let (reads, bytes) = objects.take();
        anyhow::ensure!(
            entries.len() == size,
            "the history lists {} of {size} changes",
            entries.len()
        );
        let mut warm = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            wheat_client::history::entries(&objects, &scratch.content, head, effective).await?;
            warm.push(started.elapsed().as_secs_f64());
        }
        objects.take();
        recorder.wall(&format!("{step}.load_cold"), cold);
        recorder.wall(&format!("{step}.load"), measure::median(warm));
        recorder.value(&format!("{step}.load.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.load.bytes"), bytes as f64, "bytes");
        recorder.value(
            &format!("{step}.load.reads_per_change"),
            reads as f64 / size as f64,
            "count",
        );
        recorder.value(
            &format!("{step}.load.bytes_per_change"),
            bytes as f64 / size as f64,
            "bytes",
        );
    }
    Ok(())
}
