//! The dry run of pool GC: counting what a network's pool holds that no head reaches.
//!
//! One linear history of [`LARGEST`] changes, change `i` writing a new version of file
//! `i mod FILES`, with [`FORKS`] heads each one change past the newest on a fork of its
//! own. Every publish leaves its envelope, label manifest, state and the rewritten
//! nodes of its sets behind, so most of the pool is unreachable. The dry run walks the
//! heads and lists the pool; it is counted in objects read:
//!
//! - the walk reads each reachable object once, so reads stay at or under the
//!   reachable count however much garbage the pool holds (the budget);
//! - the listing reads no objects at all.

use std::{fs, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::RepositoryEnvelopeId;
use wheat_store::{
    Durability, LocalPublicationStore, PackedObjectStore, PersistedRepositoryMutator,
    PublicationStore, SnapshotLimits, analyze_structure,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, write_at},
};

pub(crate) const NAME: &str = "structure_gc";
const FILES: usize = 1_000;
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
const FORKS: usize = 20;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(LARGEST, 200);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("files", FILES);
    recorder.parameter("forks", FORKS);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure(
        &context.scratch.join("pool"),
        changes,
        &mut recorder,
    ))?;
    Ok(recorder.finish())
}

fn path(index: usize) -> String {
    let file = index % FILES;
    format!("d{:02}/f{:02}.txt", file % 100, file / 100)
}

fn value(index: usize) -> Vec<u8> {
    format!("change {index}\n").into_bytes()
}

async fn measure(root: &Path, changes: usize, recorder: &mut Recorder) -> Result<()> {
    fs::create_dir_all(root)?;
    let history = root.join("history");
    let scratch = Scratch::open(&history)?;
    let roots = build(&scratch, changes, root).await?;
    // A deferred store publishes what it wrote ahead when it closes; a server lists
    // a durable store, opened again.
    let Scratch {
        objects, content, ..
    } = scratch;
    drop(content);
    objects.close()?;
    let objects = Counting::new(PackedObjectStore::open(history.join("objects"))?);

    let step = format!("n{changes}");
    let started = Instant::now();
    let report =
        analyze_structure(&objects, &objects.inner, &roots, SnapshotLimits::default()).await?;
    recorder.wall(&format!("{step}.analyze"), started.elapsed().as_secs_f64());
    let (reads, _) = objects.take();
    let reachable = report.total_reachable();
    let unreachable = report.total_unreachable();
    ensure!(
        report.unlisted == 0,
        "{} reached objects unlisted; {report:?}",
        report.unlisted
    );
    ensure!(unreachable.records > 0, "no garbage was left");
    recorder.value(&format!("{step}.analyze.reads"), reads as f64, "count");
    recorder.value(
        &format!("{step}.reachable.objects"),
        reachable.records as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.unreachable.objects"),
        unreachable.records as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.unreachable.share_of_bytes"),
        100.0 * unreachable.bytes as f64 / (reachable.bytes + unreachable.bytes) as f64,
        "percent",
    );
    // Each reachable object is read once, and nothing else is.
    recorder.at_most(
        &format!("{step}.analyze.reads"),
        reachable.records as f64,
        "count",
    );
    Ok(())
}

/// Writes the history and the forks; returns every head.
async fn build(
    scratch: &Scratch,
    changes: usize,
    root: &Path,
) -> Result<Vec<RepositoryEnvelopeId>> {
    let (base, mut head) = scratch.initialize().await?;
    let mutator = scratch.mutator();
    let mut effective = base;
    for index in 0..changes {
        let expected = index.checked_sub(FILES).map(value);
        (head, effective) = write_at(
            &mutator,
            &scratch.content,
            (head, base, effective),
            index,
            &path(index),
            expected.as_deref(),
            &value(index),
        )
        .await?;
    }
    let mut roots: Vec<RepositoryEnvelopeId> = vec![head.envelope];
    for fork in 0..FORKS {
        let heads = LocalPublicationStore::with_durability(
            root.join(format!("fork-{fork}.head")),
            Durability::Deferred,
        );
        heads.initialize(head)?;
        let mutator = PersistedRepositoryMutator::new(&scratch.objects, &scratch.content, &heads);
        let (forked, _) = write_at(
            &mutator,
            &scratch.content,
            (head, base, effective),
            changes + fork,
            &format!("forks/{fork}.txt"),
            None,
            &value(changes + fork),
        )
        .await?;
        roots.push(forked.envelope);
    }
    Ok(roots)
}
