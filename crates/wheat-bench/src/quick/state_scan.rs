//! A host scanning everything a clone of a long history would serve, as Gothab's
//! secret sweep does: every tree root the state retains, then every edit's content,
//! [`PAGE`] at a time through `RepositoryBrowser::state_trees` and `state_edits`.
//!
//! One linear history of [`LARGEST`] changes, change `i` writing a new version of file
//! `i mod FILES`, retains a view, a revision and an application per change. The walk
//! must read each of those records once and the radix set nodes leading to each
//! page's start: the contracts bound the objects read per record listed or visited.

use std::{fs, num::NonZeroUsize, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_store::{LocalPublicationStore, PersistedRepositoryMutator, RepositoryBrowser};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, write_at},
};

pub(crate) const NAME: &str = "state_scan";
const FILES: usize = 1_000;
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
const PAGE: usize = 1_000;
/// Objects read per record: one for the record itself, and a share of the retained
/// sets' nodes and of those each page reads on the way to its start (measured 1.10
/// for trees and 1.07 for edits at 10,000 changes; a walk rereading from the start
/// each page would be about 16).
const READS_PER_RECORD: f64 = 1.15;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(LARGEST, 200);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("files", FILES);
    recorder.parameter("page", PAGE);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure(
        &context.scratch.join("scan"),
        changes,
        &mut recorder,
    ))?;

    for walk in ["trees", "edits"] {
        recorder.at_most(
            &format!("n{LARGEST}.{walk}.reads_per_record"),
            READS_PER_RECORD,
            "count",
        );
    }
    Ok(recorder.finish())
}

async fn measure(root: &Path, changes: usize, recorder: &mut Recorder) -> Result<()> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(&root.join("history"))?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let mut effective = base;
    for index in 0..changes {
        let new = value(index);
        let expected = index.checked_sub(FILES).map(value);
        (head, effective) = write_at(
            &mutator,
            &scratch.content,
            (head, base, effective),
            index,
            &path(index),
            expected.as_deref(),
            &new,
        )
        .await?;
    }
    let step = format!("n{changes}");
    let browser = RepositoryBrowser::open(&objects, &scratch.content, head).await?;
    let page = NonZeroUsize::new(PAGE).expect("a page lists something");

    objects.take();
    let started = Instant::now();
    let (mut trees, mut pages, mut from) = (0, 0, None);
    loop {
        let listed = browser.state_trees(from, page).await?;
        trees += listed.trees.len();
        pages += 1;
        match listed.next {
            Some(next) => from = Some(next),
            None => break,
        }
    }
    recorder.wall(&format!("{step}.trees"), started.elapsed().as_secs_f64());
    let reads = objects.take().0;
    // The base view, and a view, revision and application per change.
    ensure!(
        trees == 1 + 3 * changes,
        "{trees} trees listed for {changes} changes"
    );
    record(
        recorder,
        &format!("{step}.trees"),
        trees,
        pages,
        reads,
        trees,
    );

    let started = Instant::now();
    let (mut edits, mut pages, mut from) = (0, 0, None);
    loop {
        let listed = browser.state_edits(from, page).await?;
        edits += listed.edits.len();
        pages += 1;
        match listed.next {
            Some(next) => from = Some(next),
            None => break,
        }
    }
    recorder.wall(&format!("{step}.edits"), started.elapsed().as_secs_f64());
    let reads = objects.take().0;
    ensure!(
        edits == changes,
        "{edits} edits listed for {changes} changes"
    );
    // Each revision and application is visited, and each edit read.
    let visited = 2 * changes + edits;
    record(
        recorder,
        &format!("{step}.edits"),
        edits,
        pages,
        reads,
        visited,
    );

    drop(browser);
    fs::remove_dir_all(root)?;
    Ok(())
}

fn record(
    recorder: &mut Recorder,
    step: &str,
    listed: usize,
    pages: usize,
    reads: u64,
    records: usize,
) {
    recorder.value(&format!("{step}.listed"), listed as f64, "count");
    recorder.value(&format!("{step}.pages"), pages as f64, "count");
    recorder.value(&format!("{step}.reads"), reads as f64, "count");
    recorder.value(
        &format!("{step}.reads_per_record"),
        reads as f64 / records as f64,
        "count",
    );
}

/// The file change `index` writes.
fn path(index: usize) -> String {
    let file = index % FILES;
    format!("d{:02}/f{:02}.txt", file % 100, file / 100)
}

/// What change `index` writes.
fn value(index: usize) -> Vec<u8> {
    format!("change {index}\n").into_bytes()
}
