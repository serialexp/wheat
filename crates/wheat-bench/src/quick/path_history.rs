//! A file's and a directory's history in a long linear history, as a host's history
//! page for a path reads it.
//!
//! The history has Gothab's shape: change `i` rewrites file `i mod FILES`, and file
//! `g` sits in directory `g mod DIRECTORIES`, so each file changes every
//! [`FILES`] changes and each directory every [`DIRECTORIES`]. Changes are created on
//! the frontier and published one at a time. At each checkpoint a fresh
//! `wheat_store::PathHistoryIndex` catches up to the tip (the objects it reads, per
//! change), then a file's whole history and a directory's first page are read
//! [`SAMPLES`] times each, recording the objects the first read takes and the median
//! wall time. At [`LARGEST`] changes the index finds a path's changes without reading
//! the ones between them: a page reads at most [`READS_PER_HIT`] objects per change it
//! holds, catching up at most [`READS_PER_CHANGE`] per change.

use std::{fs, num::NonZeroUsize, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{
    ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition, FileKind,
    FileValue, RevisionEdits, RevisionMetadata, ViewId,
};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, PathHistoryIndex,
    PersistedRepositoryMutator, PublicationStore, RepositoryBrowser, RepositoryHead,
    RepositoryReader,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, author, change_id, committed_at},
};
use crate::measure;

pub(crate) const NAME: &str = "path_history";
const SAMPLES: usize = 5;
const FILES: usize = 1_000;
const DIRECTORIES: usize = 100;
/// The file and the directory whose histories are read.
const FILE: usize = 0;
const DIRECTORY: usize = 1;
/// Changes on a directory's first page, as Gothab's history page shows.
const DIRECTORY_PAGE: usize = 30;
/// The history size the budgets are checked at.
const LARGEST: usize = 20_000;
/// An item's application, revision, change and change-map node or two, and the
/// membership lookup in the view's application map.
const READS_PER_HIT: f64 = 12.0;
/// An application, a revision, and the tree diff: two directory nodes each side.
const READS_PER_CHANGE: f64 = 8.0;
const PAGE_BUDGET_SECONDS: f64 = 0.05;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(vec![2_000, LARGEST], vec![150, 300]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("files", FILES);
    recorder.parameter("directories", DIRECTORIES);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_paths(
        &context.scratch.join("history"),
        &checkpoints,
        &mut recorder,
    ))?;

    let hits = LARGEST.div_ceil(FILES);
    recorder.at_most(
        &format!("n{LARGEST}.catch_up.reads"),
        READS_PER_CHANGE * LARGEST as f64,
        "count",
    );
    recorder.at_most(
        &format!("n{LARGEST}.file.reads"),
        READS_PER_HIT * hits as f64 + 10.0,
        "count",
    );
    recorder.at_most(
        &format!("n{LARGEST}.directory.reads"),
        READS_PER_HIT * (DIRECTORY_PAGE + 1) as f64 + 10.0,
        "count",
    );
    for page in ["file", "directory"] {
        recorder.at_most(&format!("n{LARGEST}.{page}.wall"), PAGE_BUDGET_SECONDS, "s");
    }
    Ok(recorder.finish())
}

async fn measure_paths(root: &Path, checkpoints: &[usize], recorder: &mut Recorder) -> Result<()> {
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
        (head, effective) =
            rewrite_at(&mutator, &scratch.content, head, base, effective, index).await?;
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let index_file = root.join(format!("paths-{size}.sqlite"));
        let paths = PathHistoryIndex::open(&index_file)?;
        let reader = RepositoryReader::new(&objects, &scratch.content);
        let view = reader.view(effective).await?;
        objects.take();
        let started = Instant::now();
        let indexed = paths.catch_up(&reader, &view).await?;
        let wall = started.elapsed().as_secs_f64();
        let reads = objects.take().0;
        ensure!(indexed == size, "catching up indexed {indexed} of {size}");
        recorder.wall(&format!("n{size}.catch_up"), wall);
        recorder.value(&format!("n{size}.catch_up.reads"), reads as f64, "count");

        let browser = RepositoryBrowser::open(&objects, &scratch.content, head).await?;
        let file_hits = size.div_ceil(FILES);
        for (name, path, page, expected) in [
            ("file", file_path(FILE), file_hits, file_hits),
            (
                "directory",
                directory_path(DIRECTORY),
                DIRECTORY_PAGE,
                DIRECTORY_PAGE.min((size + DIRECTORIES - 1 - DIRECTORY) / DIRECTORIES),
            ),
        ] {
            let step = format!("n{size}.{name}");
            let page = NonZeroUsize::new(page).expect("a page holds changes");
            objects.take();
            let mut walls = Vec::with_capacity(SAMPLES);
            let mut reads = 0;
            for sample in 0..SAMPLES {
                let started = Instant::now();
                let history = browser
                    .path_history(&paths, effective, &path, None, page)
                    .await?;
                walls.push(started.elapsed().as_secs_f64());
                ensure!(
                    history.items.len() == expected,
                    "{path}'s page holds {} changes, not {expected}",
                    history.items.len()
                );
                if sample == 0 {
                    reads = objects.take().0;
                }
            }
            objects.take();
            recorder.wall(&step, measure::median(walls));
            recorder.value(&format!("{step}.reads"), reads as f64, "count");
        }
        drop(paths);
        fs::remove_file(&index_file)?;
    }
    Ok(())
}

fn directory_path(directory: usize) -> String {
    format!("d{directory:02}")
}

/// File `file`, in directory `file mod DIRECTORIES`.
fn file_path(file: usize) -> String {
    format!(
        "{}/f{:02}.txt",
        directory_path(file % DIRECTORIES),
        file / DIRECTORIES
    )
}

/// What change `index` writes to its file.
fn value(index: usize) -> String {
    format!("change {index}\n")
}

/// Creates change number `index`, rewriting file `index mod FILES`, on the frontier of
/// `effective` and publishes it. Returns the new head and view.
async fn rewrite_at<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    content: &ContentStore,
    head: RepositoryHead,
    base: ViewId,
    effective: ViewId,
    index: usize,
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let value_of = |bytes: &str| FileValue {
        content: ContentId::of(bytes.as_bytes()),
        kind: FileKind::Regular,
    };
    let new = value(index);
    content.put(new.as_bytes())?;
    let record = EditRecord::new(
        file_path(index % FILES),
        EditOperation::ExactFile {
            expected: index
                .checked_sub(FILES)
                .map(|previous| value_of(&value(previous))),
            replacement: Some(value_of(&new)),
        },
    )?;
    let id = record.id();
    let (prepared, outcome) = mutator
        .create_on_frontier(
            head,
            base,
            effective,
            ChangeProposal {
                id: change_id(index),
                metadata: ChangeMetadata {
                    title: format!("Change {index}"),
                    creator: author(),
                    ..ChangeMetadata::default()
                },
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata {
                    author: author(),
                    committer: author(),
                    committed_at: committed_at(index),
                    ..RevisionMetadata::default()
                },
                edits: RevisionEdits {
                    edits: [(id, record)].into(),
                    transition: EditTransition {
                        added: [id].into(),
                        ..EditTransition::default()
                    },
                },
            },
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}
