//! Validating a one-change publication as the history behind it grows.
//!
//! Changes are created on the frontier and published one at a time, the way
//! `wheat change new` does, with their files spread over two levels of directories.
//! At each checkpoint the last publication is validated as an increment on the one
//! before: the first run (nothing of it read yet in this process) and the median of
//! [`SAMPLES`] more, with the objects and bytes the validator read. The mean
//! create-and-publish time of the [`CREATE_WINDOW`] changes up to each checkpoint is
//! recorded too, and the slowest single one since the checkpoint before, which shows
//! store work that only some publications pay, and the objects the last create read
//! (the frontier, the order index's skip chain to place the new application, and the
//! paths the new records are written along).
//!
//! The validator reads a root-to-leaf path in each collection a change touches and
//! walks the order index's skip chain, so its reads grow with the logarithm of the
//! history: reads per bit of history size (log2 of the change count) may grow by at
//! most [`READ_GROWTH_PER_BIT_PERCENT`] from the first checkpoint to [`LARGEST`], and
//! the same holds for what a create reads. Bytes grow faster than that in this shape:
//! the second-level directories gain an entry every 256 changes, so the directory
//! records on a change's path get larger with the history; they have the looser
//! [`BYTE_GROWTH_PER_BIT_PERCENT`]. What matters for people is the wall time: a
//! one-change publication after [`LARGEST`] changes validates within
//! [`VALIDATE_BUDGET_SECONDS`].
//!
//! The same is recorded, without a contract, for a history whose files all sit in one
//! directory (`flat.` steps, to [`FLAT_LARGEST`] changes): a directory is one record,
//! so a change to it reads and writes all of its entries.

use std::{collections::VecDeque, fs, path::Path, time::Instant};

use anyhow::Result;
use wheat_store::{
    IncrementBudget, LocalPublicationStore, PersistedRepositoryMutator, RepositoryReader,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, create_at},
};
use crate::measure;

pub(crate) const NAME: &str = "increment_validate";
/// Warm validations after the first, per checkpoint.
const SAMPLES: usize = 5;
/// Creates before each checkpoint whose mean time is recorded.
const CREATE_WINDOW: usize = 100;
/// The history size the budgets are checked at.
const LARGEST: usize = 100_000;
/// The largest flat history: every create rewrites the one directory, so building it
/// costs the square of its size.
const FLAT_LARGEST: usize = 10_000;
const VALIDATE_BUDGET_SECONDS: f64 = 0.1;
/// Growth of object reads per bit of history size, first checkpoint to [`LARGEST`].
/// Measured 2026-10-21 (release): validation 119 reads at 1,000 changes and 157 at
/// 100,000, 79%.
const READ_GROWTH_PER_BIT_PERCENT: f64 = 125.0;
/// Growth of bytes read per bit of history size: 305 KB at 1,000 changes and 601 KB at
/// 100,000 on 2026-10-21, 118%.
const BYTE_GROWTH_PER_BIT_PERCENT: f64 = 150.0;

/// What one checkpoint measured.
struct Checkpoint {
    size: usize,
    validate_reads: u64,
    validate_bytes: u64,
    create_reads: u64,
}

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 10_000, LARGEST], vec![CREATE_WINDOW, 300]);
    let flat = context
        .scale
        .pick(vec![1_000, FLAT_LARGEST], vec![CREATE_WINDOW]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("flat_checkpoints", &flat);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let spread = runtime.block_on(measure_history(
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

    recorder.at_most(
        &format!("n{LARGEST}.validate.wall"),
        VALIDATE_BUDGET_SECONDS,
        "s",
    );
    if let (Some(first), Some(last)) = (spread.first(), spread.last())
        && last.size == LARGEST
    {
        let growth = |of: fn(&Checkpoint) -> u64| {
            let per_bit =
                |checkpoint: &Checkpoint| of(checkpoint) as f64 / (checkpoint.size as f64).log2();
            per_bit(last) / per_bit(first) * 100.0
        };
        for (name, value, budget) in [
            (
                "validate.read_growth_per_bit",
                growth(|c| c.validate_reads),
                READ_GROWTH_PER_BIT_PERCENT,
            ),
            (
                "validate.byte_growth_per_bit",
                growth(|c| c.validate_bytes),
                BYTE_GROWTH_PER_BIT_PERCENT,
            ),
            (
                "create.read_growth_per_bit",
                growth(|c| c.create_reads),
                READ_GROWTH_PER_BIT_PERCENT,
            ),
        ] {
            recorder.value(name, value, "percent");
            recorder.at_most(name, budget, "percent");
        }
    }
    Ok(recorder.finish())
}

async fn measure_history(
    root: &Path,
    layout: Layout,
    prefix: &str,
    checkpoints: &[usize],
    recorder: &mut Recorder,
) -> Result<Vec<Checkpoint>> {
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
    let mut measured = Vec::with_capacity(checkpoints.len());
    let mut effective = base;
    let mut creates = VecDeque::with_capacity(CREATE_WINDOW);
    let mut slowest = 0.0_f64;
    let started = Instant::now();
    for index in 0..largest {
        let size = index + 1;
        let checkpoint = checkpoints.contains(&size);
        if checkpoint {
            objects.take();
        }
        let created = Instant::now();
        let previous = head.envelope;
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
        if creates.len() == CREATE_WINDOW {
            creates.pop_front();
        }
        let create = created.elapsed().as_secs_f64();
        creates.push_back(create);
        slowest = slowest.max(create);
        if !checkpoint {
            continue;
        }
        let (create_reads, _) = objects.take();
        let step = format!("{prefix}n{size}");
        recorder.wall(&format!("{step}.build"), started.elapsed().as_secs_f64());
        recorder.wall(
            &format!("{step}.create_mean"),
            creates.iter().sum::<f64>() / creates.len() as f64,
        );
        recorder.wall(&format!("{step}.create_max"), slowest);
        slowest = 0.0;
        recorder.value(
            &format!("{step}.create.reads"),
            create_reads as f64,
            "count",
        );

        let first = Instant::now();
        let report = reader
            .validate_increment(previous, head.envelope, IncrementBudget::STANDARD)
            .await?;
        recorder.wall(
            &format!("{step}.validate_first"),
            first.elapsed().as_secs_f64(),
        );
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.validate.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.validate.bytes"), bytes as f64, "bytes");
        recorder.value(
            &format!("{step}.budget.units"),
            report.budget_units as f64,
            "count",
        );
        recorder.value(
            &format!("{step}.budget.allowance"),
            report.budget_allowance as f64,
            "count",
        );
        recorder.value(
            &format!("{step}.budget.spent"),
            report.budget_spent as f64,
            "count",
        );
        let mut walls = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let sample = Instant::now();
            reader
                .validate_increment(previous, head.envelope, IncrementBudget::STANDARD)
                .await?;
            walls.push(sample.elapsed().as_secs_f64());
        }
        objects.take();
        recorder.wall(&format!("{step}.validate"), measure::median(walls));
        measured.push(Checkpoint {
            size,
            validate_reads: reads,
            validate_bytes: bytes,
            create_reads,
        });
        eprintln!(
            "{NAME}: {prefix}{size} changes after {:.0} s",
            started.elapsed().as_secs_f64()
        );
    }
    Ok(measured)
}
