//! Listing and copying a served repository's files, as a host backing it up or
//! moving it does while it holds the repository quiesced, and opening the copy.
//!
//! Listing reads each directory once and each file's metadata once; copying reads and
//! writes each listed byte once, then syncs the filesystem. The case builds one
//! repository with four times the files of another and records both. It has no
//! budget: a packed store keeps its objects in a few segments, so listing takes
//! microseconds at either size, and the copy's time is mostly the sync, which depends
//! on what else the disk is doing. Opening the copy rebuilds the store's locator
//! index, which is not copied (opening a packed store costs O(store) today, a TODO of
//! its own).

use std::{path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_store::{
    Durability, LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryDirectory,
};

use super::{CaseContext, CaseOutput, Recorder, history::Scratch, snapshot_export};
use crate::measure;

pub(crate) const NAME: &str = "repository_copy";
/// The large repository has this many times the files of the small one.
const GROWTH: usize = 4;
/// Listing and copying each run this many times; the median counts.
const SAMPLES: usize = 3;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(10_000, 100);
    let changes = context.scale.pick(50, 3);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("small_files", files);
    recorder.parameter("changes", changes);
    recorder.parameter("growth", GROWTH);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_size(
        &context.scratch.join("small"),
        files,
        changes,
        "small",
        &mut recorder,
    ))?;
    runtime.block_on(measure_size(
        &context.scratch.join("large"),
        files * GROWTH,
        changes,
        "large",
        &mut recorder,
    ))?;
    Ok(recorder.finish())
}

async fn measure_size(
    root: &Path,
    files: usize,
    changes: usize,
    step: &str,
    recorder: &mut Recorder,
) -> Result<()> {
    let source = RepositoryDirectory::new(root.join("source"));
    let objects = PackedObjectStore::open_deferred(source.storage())?;
    let scratch = Scratch {
        content: objects.content_store(),
        heads: LocalPublicationStore::with_durability(source.head(), Durability::Deferred),
        objects,
    };
    let started = Instant::now();
    snapshot_export::build(&scratch, files, changes).await?;
    let head = scratch.heads.read()?;
    // Closing a deferred store publishes what it wrote ahead into its segments.
    let Scratch {
        objects, content, ..
    } = scratch;
    drop(content);
    objects.close()?;
    recorder.wall(&format!("{step}.build"), started.elapsed().as_secs_f64());

    // Open, as a quiesced server holds it.
    let served = source.open_store()?;
    let mut walls = Vec::with_capacity(SAMPLES);
    let mut listed = Vec::new();
    for _ in 0..SAMPLES {
        let started = Instant::now();
        listed = source.files()?;
        walls.push(started.elapsed().as_secs_f64());
    }
    let list = measure::median(walls);
    let bytes: u64 = listed.iter().map(|file| file.len).sum();
    ensure!(
        listed.iter().any(|file| file.immutable),
        "the listing holds no sealed segment: {listed:?}"
    );
    recorder.wall(&format!("{step}.list"), list);
    recorder.value(&format!("{step}.files"), listed.len() as f64, "count");
    recorder.value(
        &format!("{step}.immutable_files"),
        listed.iter().filter(|file| file.immutable).count() as f64,
        "count",
    );
    recorder.value(&format!("{step}.bytes"), bytes as f64, "bytes");

    let mut walls = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        let copy = RepositoryDirectory::new(root.join(format!("copy{sample}")));
        let started = Instant::now();
        let copied = source.copy_to(&copy)?;
        walls.push(started.elapsed().as_secs_f64());
        ensure!(copied == listed, "the copy listed different files");
    }
    let copy_wall = measure::median(walls);
    recorder.wall(&format!("{step}.copy"), copy_wall);

    let copy = RepositoryDirectory::new(root.join("copy0"));
    let started = Instant::now();
    let reopened = copy.open_store()?;
    recorder.wall(
        &format!("{step}.open_copy"),
        started.elapsed().as_secs_f64(),
    );
    ensure!(
        copy.publications().read()? == head,
        "the copy's head differs"
    );
    drop(reopened);
    drop(served);

    Ok(())
}
