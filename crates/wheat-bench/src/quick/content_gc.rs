//! Gothab's content GC: marking the heads of networks that share history, and
//! sweeping a blob store whose content is mostly dead.
//!
//! One linear history of [`LARGEST`] changes, change `i` writing a new version of file
//! `i mod FILES`, keeps a head every [`HEAD_EVERY`] changes (`spread`), and [`FORKS`]
//! heads each one change past the newest on a fork of its own (`forks`). Marking them
//! is counted in objects read:
//!
//! - `spread`, all at once, against marking each head into a file of its own: the
//!   heads are far apart and content IDs random, so many leaves differ, but what is
//!   shared is read once.
//! - `forks`, all at once with the newest, against the newest alone: each fork adds
//!   only the nodes on its change's path.
//!
//! The sweep lists the live content and [`DEAD_PER_LIVE`] times as much that no head
//! names, asks the inventory about every marked ID in batches, and reclaims exactly
//! the dead.

use std::{
    collections::BTreeSet,
    fs, io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Result, ensure};
use async_trait::async_trait;
use wheat_core::{ContentId, RepositoryEnvelopeId};
use wheat_store::{
    BlobPage, BlobRead, ContentBlobStore, ContentInventory, ContentMarks, Durability,
    ImmutableObjectStore, LocalPublicationStore, PersistedRepositoryMutator, PublicationStore,
    RecordReferences, RepositoryReader, sweep,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, write_at},
};

pub(crate) const NAME: &str = "content_gc";
const FILES: usize = 1_000;
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
const HEAD_EVERY: usize = 500;
const FORKS: usize = 20;
const DEAD_PER_LIVE: usize = 10;
/// Marked IDs the sweep asks about per inventory call (`content_gc::REFERENCE_BATCH`).
const REFERENCE_BATCH: usize = 1024;
/// Marking the spread heads at once may read this share of marking each alone
/// (measured 51% at 10,000 changes).
const SPREAD_SHARE_PERCENT: f64 = 60.0;
/// Each fork may add this many reads to marking the newest alone: its envelope,
/// state and the new nodes on its change's path.
const READS_PER_FORK: f64 = 10.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let (changes, head_every) = context.scale.pick((LARGEST, HEAD_EVERY), (200, 50));
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("files", FILES);
    recorder.parameter("head_every", head_every);
    recorder.parameter("forks", FORKS);
    recorder.parameter("dead_per_live", DEAD_PER_LIVE);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let measured = runtime.block_on(measure(
        &context.scratch.join("gc"),
        changes,
        head_every,
        &mut recorder,
    ))?;

    let step = format!("n{LARGEST}");
    recorder.at_most(
        &format!("{step}.spread.share_of_separate"),
        SPREAD_SHARE_PERCENT,
        "percent",
    );
    recorder.at_most(
        &format!("{step}.forks.reads"),
        measured.newest_reads as f64 + READS_PER_FORK * FORKS as f64,
        "count",
    );
    recorder.at_most(
        &format!("{step}.sweep.reference_calls"),
        (measured.marked as f64 / REFERENCE_BATCH as f64).ceil(),
        "count",
    );
    // One reclaim call per listed page at most, not one per ID.
    recorder.at_most(
        &format!("{step}.sweep.reclaim_calls"),
        (measured.listed as f64 / wheat_store::SWEEP_PAGE_IDS as f64).ceil(),
        "count",
    );
    Ok(recorder.finish())
}

struct Measured {
    newest_reads: u64,
    marked: u64,
    listed: u64,
}

/// Marks `heads` into a new file at `path`: the reads and the file.
async fn mark<O: ImmutableObjectStore>(
    objects: &Counting,
    reader: &RepositoryReader<'_, O>,
    path: &Path,
    heads: &[RepositoryEnvelopeId],
) -> Result<(u64, wheat_store::MarkCounts, ContentMarks)> {
    objects.take();
    let mut marks = ContentMarks::create(path, std::time::SystemTime::now())?;
    let counts = marks.mark_heads(reader, heads.iter().copied()).await?;
    Ok((objects.take().0, counts, marks))
}

/// Builds the history, marks and sweeps.
async fn measure(
    root: &Path,
    changes: usize,
    head_every: usize,
    recorder: &mut Recorder,
) -> Result<Measured> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(&root.join("history"))?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let mut spread: Vec<RepositoryEnvelopeId> = Vec::new();
    let mut live = BTreeSet::new();
    let mut effective = base;
    for index in 0..changes {
        let new = value(index);
        let expected = index.checked_sub(FILES).map(value);
        live.insert(ContentId::of(&new));
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
        if (index + 1) % head_every == 0 {
            spread.push(head.envelope);
        }
    }
    let newest = head.envelope;
    // Each fork publishes one change of its own past the newest head.
    let mut forks = vec![newest];
    for fork in 0..FORKS {
        let index = changes + fork;
        let heads = LocalPublicationStore::with_durability(
            root.join(format!("fork-{fork}.head")),
            Durability::Deferred,
        );
        heads.initialize(head)?;
        let mutator = PersistedRepositoryMutator::new(&objects, &scratch.content, &heads);
        let new = value(index);
        live.insert(ContentId::of(&new));
        let (forked, _) = write_at(
            &mutator,
            &scratch.content,
            (head, base, effective),
            index,
            &format!("forks/{fork}.txt"),
            None,
            &new,
        )
        .await?;
        forks.push(forked.envelope);
    }
    let step = format!("n{changes}");
    let reader = RepositoryReader::new(&objects, &scratch.content);

    let started = Instant::now();
    let (newest_reads, alone, _) = mark(&objects, &reader, &root.join("newest"), &[newest]).await?;
    recorder.wall(&format!("{step}.newest"), started.elapsed().as_secs_f64());
    recorder.value(
        &format!("{step}.newest.reads"),
        newest_reads as f64,
        "count",
    );

    let mut separate = 0;
    for (n, head) in spread.iter().enumerate() {
        separate += mark(
            &objects,
            &reader,
            &root.join(format!("alone-{n}")),
            &[*head],
        )
        .await?
        .0;
    }
    let started = Instant::now();
    let (spread_reads, counts, _) = mark(&objects, &reader, &root.join("spread"), &spread).await?;
    recorder.wall(&format!("{step}.spread"), started.elapsed().as_secs_f64());
    recorder.value(
        &format!("{step}.spread.reads"),
        spread_reads as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.spread.separate_reads"),
        separate as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.spread.share_of_separate"),
        100.0 * spread_reads as f64 / separate as f64,
        "percent",
    );
    recorder.value(
        &format!("{step}.spread.nodes_skipped"),
        counts.nodes_skipped as f64,
        "count",
    );
    ensure!(
        counts.marked == alone.marked,
        "every spread head's content is in the newest one's: {} marked, {} alone",
        counts.marked,
        alone.marked
    );

    let started = Instant::now();
    let (fork_reads, counts, all) = mark(&objects, &reader, &root.join("forks"), &forks).await?;
    recorder.wall(&format!("{step}.forks"), started.elapsed().as_secs_f64());
    recorder.value(&format!("{step}.forks.reads"), fork_reads as f64, "count");
    ensure!(
        counts.marked == alone.marked + FORKS as u64,
        "the forks add one version each: {} marked, {} in the newest",
        counts.marked,
        alone.marked
    );
    ensure!(
        live.iter().all(|id| all.contains(*id).unwrap_or(false)),
        "a written version is not marked"
    );
    let marked = counts.marked;
    let path = all.finish()?;

    let dead: Vec<ContentId> = (0..live.len() * DEAD_PER_LIVE)
        .map(|index| ContentId::of(format!("dead {index}").as_bytes()))
        .collect();
    let mut listed: Vec<ContentId> = live.iter().copied().chain(dead.iter().copied()).collect();
    listed.sort_unstable();
    let blobs: Arc<dyn ContentBlobStore> = Arc::new(Listing { ids: listed });
    let inventory = Inventory::default();
    let mut marks = ContentMarks::open(&path)?;
    let started = Instant::now();
    let swept = sweep(&blobs, &mut marks, &inventory).await?;
    recorder.wall(&format!("{step}.sweep"), started.elapsed().as_secs_f64());
    for (name, value) in [
        ("listed", swept.listed),
        ("reclaimed", swept.reclaimed),
        ("reference_calls", swept.reference_calls),
        ("reclaim_calls", swept.reclaim_calls),
    ] {
        recorder.value(&format!("{step}.sweep.{name}"), value as f64, "count");
    }
    ensure!(
        swept.reclaimed == dead.len() as u64
            && inventory.reclaimed.load(Ordering::Relaxed) == dead.len() as u64,
        "the sweep reclaimed {} of {} dead",
        swept.reclaimed,
        dead.len()
    );
    fs::remove_dir_all(root)?;
    Ok(Measured {
        newest_reads,
        marked,
        listed: swept.listed,
    })
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

/// A blob store that only lists: the sweep reads no records here.
#[derive(Debug)]
struct Listing {
    ids: Vec<ContentId>,
}

#[async_trait]
impl ContentBlobStore for Listing {
    async fn put(&self, _: ContentId, _: Vec<u8>) -> io::Result<()> {
        Err(io::Error::other("the listing is read-only"))
    }

    async fn get(&self, _: ContentId) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    async fn get_range(
        &self,
        _: ContentId,
        _: std::ops::Range<u64>,
    ) -> io::Result<Option<BlobRead>> {
        Ok(None)
    }

    async fn list_page(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> io::Result<Option<BlobPage>> {
        let start = cursor.map_or(Ok(0), |cursor| {
            cursor.parse::<usize>().map_err(io::Error::other)
        })?;
        let end = (start + limit).min(self.ids.len());
        Ok(Some(BlobPage {
            ids: self.ids[start..end].to_vec(),
            next: (end < self.ids.len()).then(|| end.to_string()),
        }))
    }
}

/// Every record stored whole, so the marks are closed at once.
#[derive(Default)]
struct Inventory {
    reclaimed: AtomicU64,
}

#[async_trait]
impl ContentInventory for Inventory {
    async fn references(
        &self,
        ids: &[ContentId],
    ) -> io::Result<Vec<(ContentId, RecordReferences)>> {
        Ok(ids.iter().map(|id| (*id, RecordReferences::None)).collect())
    }

    async fn reclaim(&self, ids: &[ContentId]) -> io::Result<Vec<ContentId>> {
        self.reclaimed
            .fetch_add(ids.len() as u64, Ordering::Relaxed);
        Ok(ids.to_vec())
    }
}
