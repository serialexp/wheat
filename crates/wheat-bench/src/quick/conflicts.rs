//! A view's unresolved conflicts in a long history, as a host's conflicts page reads
//! them.
//!
//! Change 0 creates [`SHARED`] files; every tenth change after it rewrites one of them
//! and every other change rewrites one of [`FILES`] others, each from the value the
//! change before it on that file wrote. Revising change 0 to write other values makes
//! every later change on a shared file conflict, about a tenth of the history, from
//! one recomposition, whose wall time is recorded too (resolving an early conflict
//! recomposes the same way). A fresh `wheat_store::ConflictIndex` then catches up to
//! the revised view (the objects it reads, per change), and the first page, a page
//! from the middle and the conflict at one position are read [`SAMPLES`] times each,
//! recording the objects the first read takes and the median wall time. At
//! [`LARGEST`] changes a page reads at most [`READS_PER_CONFLICT`] objects per
//! conflict it holds, however long the history.

use std::{collections::HashMap, fs, num::NonZeroUsize, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition,
    FileKind, FileValue, RevisionEdits, RevisionMetadata, ViewId,
};
use wheat_store::{
    ConflictIndex, ConflictPageLimits, ContentStore, ImmutableObjectStore, LocalPublicationStore,
    PersistedRepositoryMutator, PublicationStore, RepositoryBrowser, RepositoryHead,
    RepositoryReader, RevisionMapStore,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, author, change_id, committed_at},
};
use crate::measure;

pub(crate) const NAME: &str = "conflicts";
const SAMPLES: usize = 5;
const SHARED: usize = 100;
const FILES: usize = 1_000;
/// Conflicts on a page, as a host's conflicts page shows.
const PAGE: usize = 30;
/// Failed edits a page reads at most.
const PAGE_EDITS: usize = 1_000;
/// The history size the budgets are checked at.
const LARGEST: usize = 20_000;
/// A conflict's application, revision, conflict, change, change-map node or two and
/// its failed edit.
const READS_PER_CONFLICT: f64 = 9.0;
/// The view check and the producing application's lookup in the view's application
/// map, twice (catch-up and the read).
const READS_PER_PAGE: f64 = 30.0;
/// An application, and a revision for each conflicted one.
const READS_PER_CHANGE: f64 = 2.0;
const PAGE_BUDGET_SECONDS: f64 = 0.05;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let size = context.scale.pick(LARGEST, 300);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", size);
    recorder.parameter("shared", SHARED);
    recorder.parameter("files", FILES);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_conflicts(
        &context.scratch.join("history"),
        size,
        &mut recorder,
    ))?;

    recorder.at_most(
        &format!("n{LARGEST}.catch_up.reads"),
        READS_PER_CHANGE * LARGEST as f64,
        "count",
    );
    for page in ["first_page", "middle_page"] {
        recorder.at_most(
            &format!("n{LARGEST}.{page}.reads"),
            READS_PER_CONFLICT * PAGE as f64 + READS_PER_PAGE,
            "count",
        );
        recorder.at_most(&format!("n{LARGEST}.{page}.wall"), PAGE_BUDGET_SECONDS, "s");
    }
    recorder.at_most(&format!("n{LARGEST}.at.wall"), PAGE_BUDGET_SECONDS, "s");
    Ok(recorder.finish())
}

async fn measure_conflicts(root: &Path, size: usize, recorder: &mut Recorder) -> Result<()> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let mut files = HashMap::new();
    let mut effective = base;
    let first = (0..SHARED)
        .map(|file| (shared_path(file), format!("shared {file}\n")))
        .collect::<Vec<_>>();
    (head, effective) = create(
        &mutator,
        &scratch.content,
        (head, base, effective),
        0,
        &mut files,
        &first,
    )
    .await?;
    for index in 1..size {
        let path = if index % 10 == 0 {
            shared_path(index / 10 % SHARED)
        } else {
            format!("d{:02}/f{:03}.txt", index % 100, index % FILES)
        };
        let write = [(path, format!("change {index}\n"))];
        (head, effective) = create(
            &mutator,
            &scratch.content,
            (head, base, effective),
            index,
            &mut files,
            &write,
        )
        .await?;
    }

    let started = Instant::now();
    let revised;
    (head, revised) =
        revise_first(&mutator, &objects, &scratch.content, head, base, effective).await?;
    recorder.wall(
        &format!("n{size}.revise_first"),
        started.elapsed().as_secs_f64(),
    );
    let conflicts = (size - 1) / 10;

    let index_file = root.join("conflicts.sqlite");
    let index = ConflictIndex::open(&index_file)?;
    let reader = RepositoryReader::new(&objects, &scratch.content);
    let view = reader.view(revised).await?;
    objects.take();
    let started = Instant::now();
    let indexed = index.catch_up(&reader, &view).await?;
    let wall = started.elapsed().as_secs_f64();
    let reads = objects.take().0;
    ensure!(indexed == size, "catching up indexed {indexed} of {size}");
    recorder.wall(&format!("n{size}.catch_up"), wall);
    recorder.value(&format!("n{size}.catch_up.reads"), reads as f64, "count");
    let count = index.count(&reader, &view).await?;
    ensure!(
        count == conflicts as u64,
        "the view has {count} conflicts, not {conflicts}"
    );
    recorder.value(&format!("n{size}.conflicts"), count as f64, "count");

    let browser = RepositoryBrowser::open(&objects, &scratch.content, head).await?;
    let limits = ConflictPageLimits {
        conflicts: NonZeroUsize::new(PAGE).expect("a page holds conflicts"),
        edits: NonZeroUsize::new(PAGE_EDITS).expect("a page holds edits"),
    };
    let middle = Some(size as u64 / 2);
    for (name, after) in [("first_page", None), ("middle_page", middle)] {
        let step = format!("n{size}.{name}");
        // The conflicts are at the tenth positions from 10 on.
        let past = conflicts - after.map_or(0, |after| after as usize / 10);
        let expected = PAGE.min(past);
        objects.take();
        let mut walls = Vec::with_capacity(SAMPLES);
        let mut reads = 0;
        for sample in 0..SAMPLES {
            let started = Instant::now();
            let page = browser.conflicts(&index, revised, after, limits).await?;
            walls.push(started.elapsed().as_secs_f64());
            ensure!(
                page.items.len() == expected,
                "{name} holds {} conflicts, not {expected}",
                page.items.len()
            );
            if sample == 0 {
                reads = objects.take().0;
            }
        }
        objects.take();
        recorder.wall(&step, measure::median(walls));
        recorder.value(&format!("{step}.reads"), reads as f64, "count");
    }

    let position = (size as u64 / 2).next_multiple_of(10);
    let mut walls = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let conflict = index.at(&reader, &view, position).await?;
        walls.push(started.elapsed().as_secs_f64());
        ensure!(conflict.is_some(), "no conflict at {position}");
    }
    recorder.wall(&format!("n{size}.at"), measure::median(walls));
    drop(index);
    fs::remove_file(&index_file)?;
    Ok(())
}

fn shared_path(file: usize) -> String {
    format!("shared/s{file:03}.txt")
}

fn value_of(bytes: &str) -> FileValue {
    FileValue {
        content: ContentId::of(bytes.as_bytes()),
        kind: FileKind::Regular,
    }
}

/// Edits writing each `(path, value)` over the value `files` holds for it, which it
/// then holds the new one for.
fn rewrites(
    content: &ContentStore,
    files: &mut HashMap<String, String>,
    writes: &[(String, String)],
) -> Result<RevisionEdits> {
    let mut edits = RevisionEdits {
        edits: Default::default(),
        transition: EditTransition::default(),
    };
    for (path, new) in writes {
        content.put(new.as_bytes())?;
        let record = EditRecord::new(
            path.clone(),
            EditOperation::ExactFile {
                expected: files.get(path).map(|old| value_of(old)),
                replacement: Some(value_of(new)),
            },
        )?;
        let id = record.id();
        edits.edits.insert(id, record);
        edits.transition.added.insert(id);
        files.insert(path.clone(), new.clone());
    }
    Ok(edits)
}

/// Creates change number `index` writing `writes` on the frontier of `effective` and
/// publishes it. Returns the new head and view.
async fn create<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    content: &ContentStore,
    (head, base, effective): (RepositoryHead, ViewId, ViewId),
    index: usize,
    files: &mut HashMap<String, String>,
    writes: &[(String, String)],
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let edits = rewrites(content, files, writes)?;
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
                edits,
            },
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}

/// Revises change 0 to write other values to the shared files, which every later
/// change on them expected, and publishes it. Returns the new head and view.
async fn revise_first<O, P>(
    mutator: &PersistedRepositoryMutator<'_, O, P>,
    objects: &O,
    content: &ContentStore,
    head: RepositoryHead,
    base: ViewId,
    effective: ViewId,
) -> Result<(RepositoryHead, ViewId)>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let first: ChangeId = change_id(0);
    let reader = RepositoryReader::new(objects, content);
    let view = reader.view(effective).await?;
    let predecessor = RevisionMapStore::new(objects)
        .get(view.revisions, first)
        .await?
        .ok_or_else(|| anyhow::anyhow!("change 0 is not in the view"))?;
    let old = reader.revision(predecessor).await?;
    let writes = (0..SHARED)
        .map(|file| (shared_path(file), format!("revised shared {file}\n")))
        .collect::<Vec<_>>();
    let mut edits = rewrites(content, &mut HashMap::new(), &writes)?;
    edits.transition.removed.extend(old.edits.iter().copied());
    let (prepared, outcome) = mutator
        .revise(
            head,
            base,
            effective,
            first,
            predecessor,
            RevisionMetadata {
                author: author(),
                committer: author(),
                committed_at: committed_at(0),
                ..RevisionMetadata::default()
            },
            edits,
        )
        .await?;
    Ok((mutator.publish(prepared).await?, outcome.resulting_view))
}
