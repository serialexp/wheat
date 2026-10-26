//! A label's descendant frontiers while work piles up on a label left in place.
//!
//! Changes are created one at a time on the frontier of a label at the base view, the
//! way `wheat change new` does without a publish to move the label, so every view since
//! lies below the label. At each checkpoint three ways of finding the label's one
//! frontier are measured, each with the objects and bytes it read: the forward walk
//! (`ViewLineageMapStore::descendant_frontiers`), which visits every view below the
//! label; the store's frontier index (`FrontierIndex`) built for the label from
//! nothing, which is that walk once; and the index after one more change, which reads
//! the lineage diff that change made and the new view's children.
//!
//! The last is what `switch`, `refresh`, `publish`, `fetch`, `integrate` and
//! `label move` pay on a store whose index is warm. It reads at most
//! [`WARM_READS_BUDGET`] objects at [`LARGEST`], and its reads may grow by at most
//! [`WARM_READ_GROWTH_PER_BIT_PERCENT`] per bit of history size from the first
//! checkpoint to [`LARGEST`], as the lineage map deepens; the walk's grow with the
//! history itself.

use std::{fs, path::Path, time::Instant};

use anyhow::Result;
use wheat_core::ViewLineageMapId;
use wheat_store::{
    FrontierIndex, ImmutableObjectStore, LocalPublicationStore, PersistedRepositoryMutator,
    RepositoryHead, RepositoryReader, ViewLineageLimits, ViewLineageMapStore,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, create_at},
};

pub(crate) const NAME: &str = "label_frontiers";
/// The history size the budgets are checked at.
const LARGEST: usize = 30_000;
/// Objects the warm index reads at [`LARGEST`]. Measured 2026-10-27 (release): 7 at
/// 1,000, 10,000 and 30,000 changes, where the walk read 1,243, 10,955 and 35,358.
const WARM_READS_BUDGET: f64 = 16.0;
/// Growth of the warm index's reads per bit of history size, first checkpoint to
/// [`LARGEST`]: 67% on 2026-10-27, since the reads did not grow at all.
const WARM_READ_GROWTH_PER_BIT_PERCENT: f64 = 125.0;

/// What one checkpoint measured.
struct Checkpoint {
    size: usize,
    warm_reads: u64,
}

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 10_000, LARGEST], vec![20, 60]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let measured = runtime.block_on(measure_history(
        &context.scratch,
        &checkpoints,
        &mut recorder,
    ))?;
    check_growth(&mut recorder, &measured);
    Ok(recorder.finish())
}

async fn measure_history(
    root: &Path,
    checkpoints: &[usize],
    recorder: &mut Recorder,
) -> Result<Vec<Checkpoint>> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(&root.join("store"))?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let reader = RepositoryReader::new(&objects, &scratch.content);
    let index = FrontierIndex::open(&root.join("frontiers.sqlite"))?;
    let limits = ViewLineageLimits::default();
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut effective = base;
    let mut measured = Vec::new();
    let started = Instant::now();
    for index_in_history in 0..largest {
        let size = index_in_history + 1;
        if checkpoints.contains(&size) {
            // Warm the index on the history before the checkpoint's last change.
            let lineage = lineage(&reader, head).await?;
            index
                .frontiers(&objects, lineage, [base], base, limits)
                .await?;
        }
        (head, effective) = create_at(
            &mutator,
            &scratch.content,
            head,
            base,
            effective,
            index_in_history,
            Layout::Spread,
        )
        .await?;
        if !checkpoints.contains(&size) {
            continue;
        }
        let step = format!("n{size}");
        let lineage = lineage(&reader, head).await?;
        objects.take();

        let warm = Instant::now();
        let frontiers = index
            .frontiers(&objects, lineage, [base], base, limits)
            .await?;
        recorder.wall(&format!("{step}.warm"), warm.elapsed().as_secs_f64());
        anyhow::ensure!(frontiers == [effective], "the index found {frontiers:?}");
        let (warm_reads, bytes) = objects.take();
        recorder.value(&format!("{step}.warm.reads"), warm_reads as f64, "count");
        recorder.value(&format!("{step}.warm.bytes"), bytes as f64, "bytes");

        let walk = Instant::now();
        let frontiers = ViewLineageMapStore::new(&objects)
            .descendant_frontiers(lineage, base, limits)
            .await?;
        recorder.wall(&format!("{step}.walk"), walk.elapsed().as_secs_f64());
        anyhow::ensure!(frontiers == [effective], "the walk found {frontiers:?}");
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.walk.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.walk.bytes"), bytes as f64, "bytes");

        let cold_index = FrontierIndex::open(&root.join(format!("cold-{size}.sqlite")))?;
        let cold = Instant::now();
        cold_index
            .frontiers(&objects, lineage, [base], base, limits)
            .await?;
        recorder.wall(&format!("{step}.cold"), cold.elapsed().as_secs_f64());
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.cold.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.cold.bytes"), bytes as f64, "bytes");

        measured.push(Checkpoint { size, warm_reads });
        eprintln!(
            "{NAME}: {size} changes after {:.0} s",
            started.elapsed().as_secs_f64()
        );
    }
    Ok(measured)
}

async fn lineage<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    head: RepositoryHead,
) -> Result<ViewLineageMapId> {
    let envelope = reader.envelope(head.envelope).await?;
    Ok(reader.state(envelope.state).await?.view_lineage)
}

/// Holds the warm index's reads to their budget at [`LARGEST`] and to the growth
/// budget from the first checkpoint to it.
fn check_growth(recorder: &mut Recorder, measured: &[Checkpoint]) {
    recorder.at_most(
        &format!("n{LARGEST}.warm.reads"),
        WARM_READS_BUDGET,
        "count",
    );
    let (Some(first), Some(last)) = (measured.first(), measured.last()) else {
        return;
    };
    if last.size != LARGEST {
        return;
    }
    let per_bit =
        |checkpoint: &Checkpoint| checkpoint.warm_reads as f64 / (checkpoint.size as f64).log2();
    let name = "warm.read_growth_per_bit";
    recorder.value(name, per_bit(last) / per_bit(first) * 100.0, "percent");
    recorder.at_most(name, WARM_READ_GROWTH_PER_BIT_PERCENT, "percent");
}
