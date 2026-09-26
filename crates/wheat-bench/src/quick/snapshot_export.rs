//! Exporting and restoring a native snapshot, at two repository sizes.
//!
//! Export walks the whole closure of the head, keeping what it has seen and what is
//! still to do in a scratch index on disk, so memory stays flat however big the
//! repository is. Restore walks the restored closure again to check it matches the
//! manifest. Both do a fixed amount of work per object and per blob, so the time per
//! item must not grow with the repository: the case builds one repository with four
//! times the files of another and compares.
//!
//! Both have the same number of changes. Every view keeps a list of all its files
//! (its content set), so growing files and changes together would grow the bytes with
//! their product, much faster than the item count. With the changes fixed, items and
//! bytes grow alike.

use std::{collections::BTreeSet, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, FileKind, FileValue, Repository,
    RevisionMetadata, Tree,
};
use wheat_store::{
    NativeSnapshot, PublicationStore, RepositoryPublisher, RepositoryReader, SnapshotLimits,
    export_snapshot, restore_snapshot,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Scratch, author, rewrite},
};
use crate::measure;

pub(crate) const NAME: &str = "snapshot_export";
/// Budget for the time per item (object or blob) of the large repository, as a
/// percentage of the same for the small one.
const PER_ITEM_GROWTH_BUDGET_PERCENT: f64 = 150.0;
/// The large repository has this many times the files of the small one.
const GROWTH: usize = 4;
const FILES_PER_DIRECTORY: usize = 25;
/// Export and restore each run this many times; the median counts.
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
    let small = runtime.block_on(measure_size(
        &context.scratch.join("small"),
        files,
        changes,
        "small",
        &mut recorder,
    ))?;
    let large = runtime.block_on(measure_size(
        &context.scratch.join("large"),
        files * GROWTH,
        changes,
        "large",
        &mut recorder,
    ))?;
    for (step, small, large) in [
        ("export", small.export, large.export),
        ("restore", small.restore, large.restore),
    ] {
        recorder.value(&format!("{step}_growth"), large / small * 100.0, "percent");
        recorder.at_most(
            &format!("{step}_growth"),
            PER_ITEM_GROWTH_BUDGET_PERCENT,
            "percent",
        );
    }
    Ok(recorder.finish())
}

/// Seconds per item.
struct PerItem {
    export: f64,
    restore: f64,
}

async fn measure_size(
    root: &Path,
    files: usize,
    changes: usize,
    step: &str,
    recorder: &mut Recorder,
) -> Result<PerItem> {
    let scratch = Scratch::open(&root.join("source"))?;
    let started = Instant::now();
    build(&scratch, files, changes).await?;
    recorder.wall(&format!("{step}.build"), started.elapsed().as_secs_f64());

    // Export and restore both end with this check; timed alone to tell it apart.
    let started = Instant::now();
    RepositoryReader::new(&scratch.objects, &scratch.content)
        .validate_envelope(scratch.heads.read()?.envelope)
        .await?;
    recorder.wall(&format!("{step}.validate"), started.elapsed().as_secs_f64());

    let limits = SnapshotLimits::default();
    let mut walls = Vec::with_capacity(SAMPLES);
    let mut snapshot: Option<NativeSnapshot> = None;
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let exported =
            export_snapshot(&scratch.objects, &scratch.content, &scratch.heads, limits).await?;
        walls.push(started.elapsed().as_secs_f64());
        if let Some(earlier) = &snapshot {
            ensure!(
                earlier.manifest.id() == exported.manifest.id(),
                "export is not repeatable"
            );
        }
        snapshot = Some(exported);
    }
    let snapshot = snapshot.expect("SAMPLES is not zero");
    let export = measure::median(walls);
    recorder.wall(&format!("{step}.export"), export);
    let items = (snapshot.manifest.objects.len() + snapshot.manifest.contents.len()) as f64;
    recorder.value(
        &format!("{step}.objects"),
        snapshot.manifest.objects.len() as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.contents"),
        snapshot.manifest.contents.len() as f64,
        "count",
    );
    let object_bytes: u64 = snapshot
        .manifest
        .objects
        .iter()
        .map(|entry| entry.bytes)
        .sum();
    recorder.value(
        &format!("{step}.object_bytes"),
        object_bytes as f64,
        "bytes",
    );

    let expected = scratch.heads.read()?;
    let mut walls = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        let target = Scratch::open(&root.join(format!("restored{sample}")))?;
        let started = Instant::now();
        let head = restore_snapshot(
            &snapshot,
            &target.objects,
            &target.content,
            &target.heads,
            limits,
        )
        .await?;
        walls.push(started.elapsed().as_secs_f64());
        ensure!(head == expected, "the restored head differs");
    }
    let restore = measure::median(walls);
    recorder.wall(&format!("{step}.restore"), restore);

    recorder.value(
        &format!("{step}.export_per_item"),
        export / items * 1e3,
        "ms",
    );
    recorder.value(
        &format!("{step}.restore_per_item"),
        restore / items * 1e3,
        "ms",
    );
    Ok(PerItem {
        export: export / items,
        restore: restore / items,
    })
}

fn value(index: usize) -> Vec<u8> {
    format!("value {index}\n").into_bytes()
}

/// A base of `files` files, 25 to a directory, then a stack of `changes` one-file
/// changes, each published on its own.
async fn build(scratch: &Scratch, files: usize, changes: usize) -> Result<()> {
    let mut tree = Tree::new();
    for index in 0..files {
        let bytes = value(index);
        scratch.content.put(&bytes)?;
        tree.insert(
            format!("d{:05}/f{index:07}.txt", index / FILES_PER_DIRECTORY),
            FileValue {
                content: ContentId::of(&bytes),
                kind: FileKind::Regular,
            },
        );
    }
    let repository = Repository::new(tree);
    let base = repository.base_view().id;
    let mut head = RepositoryPublisher::new(&scratch.objects, &scratch.content, &scratch.heads)
        .initialize(&repository)
        .await?;

    let author = author();
    let mutator = scratch.mutator();
    let mut effective = base;
    for index in 0..changes {
        let (_, edits) = rewrite(
            &scratch.content,
            &format!("changes/c{index:06}.txt"),
            &value(files + index),
            None,
        )?;
        let (prepared, outcome) = mutator
            .create(
                head,
                base,
                effective,
                ChangeProposal {
                    id: ChangeId::new(),
                    metadata: ChangeMetadata {
                        title: format!("Change {index}"),
                        creator: author.clone(),
                        ..ChangeMetadata::default()
                    },
                    dependencies: BTreeSet::new(),
                    revision_metadata: RevisionMetadata {
                        message: format!("Change {index}"),
                        author: author.clone(),
                        committer: author.clone(),
                        ..RevisionMetadata::default()
                    },
                    edits,
                },
            )
            .await?;
        head = mutator.publish(prepared).await?;
        effective = outcome.resulting_view;
    }
    Ok(())
}
