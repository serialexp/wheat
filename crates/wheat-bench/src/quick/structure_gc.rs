//! Pool GC: counting what a network's pool holds that no head reaches, then removing
//! it.
//!
//! One linear history of [`LARGEST`] changes, change `i` writing a new version of file
//! `i mod FILES`, with [`FORKS`] heads each one change past the newest on a fork of its
//! own. Every publish leaves its envelope, label manifest, state and the rewritten
//! nodes of its sets behind, so most of the pool is unreachable.
//!
//! The dry run walks the heads and lists the pool; it is counted in objects read:
//!
//! - the walk reads each reachable object once, so reads stay at or under the
//!   reachable count however much garbage the pool holds (the budget);
//! - the listing reads no objects at all.
//!
//! The collection validates each head, walks them again to mark, and rewrites the
//! pool keeping what is marked. Its budgets:
//!
//! - objects read stay within one complete validation per head (measured on its
//!   own, of the largest head) plus the marking walk, however much garbage there is;
//! - the segments written hold the reachable objects and nothing else: their bytes
//!   stay within [`WRITTEN_PER_KEPT_BYTE`] of those objects' canonical bytes.

use std::{fs, path::Path, time::Instant};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use wheat_core::{ObjectId, RepositoryEnvelopeId};
use wheat_store::{
    CollectableStructure, ContentItem, ContentStore, Durability, ImmutableObjectStore,
    LocalPublicationStore, ObjectStoreError, PackedObjectStore, PersistedRepositoryMutator,
    PublicationStore, RepositoryReader, RepositoryStore, RepositoryStoreError, SnapshotLimits,
    StoredObject, StoredWrite, analyze_structure, collect_structure,
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
/// Segment bytes written per canonical byte of the objects kept: the records'
/// framing and the segment's index (measured 1.0065 at 10,000 changes).
const WRITTEN_PER_KEPT_BYTE: f64 = 1.02;

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
    let objects = Pool {
        objects: Counting::new(PackedObjectStore::open(history.join("objects"))?),
        content: ContentStore::new(history.join("content")),
    };

    let step = format!("n{changes}");
    let started = Instant::now();
    let report = analyze_structure(
        &objects.objects,
        &objects.objects.inner,
        &roots,
        SnapshotLimits::default(),
    )
    .await?;
    recorder.wall(&format!("{step}.analyze"), started.elapsed().as_secs_f64());
    let (reads, _) = objects.objects.take();
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

    // One head's complete validation, which the collection does for each. A fork's
    // head reaches the most.
    let largest = *roots.last().context("no heads")?;
    let started = Instant::now();
    RepositoryReader::new(&objects, &objects.content)
        .validate_envelope(largest)
        .await?;
    recorder.wall(
        &format!("{step}.validate_one"),
        started.elapsed().as_secs_f64(),
    );
    let (validate_reads, _) = objects.objects.take();
    recorder.value(
        &format!("{step}.validate_one.reads"),
        validate_reads as f64,
        "count",
    );

    let started = Instant::now();
    let collection = collect_structure(&objects, &roots, SnapshotLimits::default())
        .await
        .context("collecting the pool")?;
    recorder.wall(&format!("{step}.collect"), started.elapsed().as_secs_f64());
    let (reads, _) = objects.objects.take();
    let sweep = collection.sweep;
    ensure!(
        sweep.objects_dropped + sweep.loose_objects_retired == unreachable.records
            && collection.marked == reachable.records,
        "the collection removed other than the dry run counted: {collection:?}, {report:?}"
    );
    let after = analyze_structure(
        &objects.objects,
        &objects.objects.inner,
        &roots,
        SnapshotLimits::default(),
    )
    .await?;
    objects.objects.take();
    ensure!(
        after.total_unreachable().records == 0 && after.total_reachable() == reachable,
        "the pool after the collection: {after:?}"
    );
    recorder.value(&format!("{step}.collect.reads"), reads as f64, "count");
    recorder.value(
        &format!("{step}.collect.bytes_written"),
        sweep.segment_bytes_after as f64,
        "bytes",
    );
    recorder.value(
        &format!("{step}.collect.written_per_kept_byte"),
        sweep.segment_bytes_after as f64 / reachable.bytes as f64,
        "count",
    );
    recorder.value(
        &format!("{step}.collect.bytes_removed"),
        sweep
            .segment_bytes_before
            .saturating_sub(sweep.segment_bytes_after) as f64,
        "bytes",
    );
    // A complete validation of each head, then the marking walk.
    recorder.at_most(
        &format!("{step}.collect.reads"),
        (roots.len() as u64 * validate_reads + reachable.records) as f64,
        "count",
    );
    recorder.at_most(
        &format!("{step}.collect.written_per_kept_byte"),
        WRITTEN_PER_KEPT_BYTE,
        "count",
    );
    Ok(())
}

/// The pool, counted, with the content store the history's content went to, as a
/// network's store pairs them.
struct Pool {
    objects: Counting,
    content: ContentStore,
}

#[async_trait]
impl ImmutableObjectStore for Pool {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.objects.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.objects.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.objects.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.objects.exists_many(ids).await
    }
}

#[async_trait]
impl RepositoryStore for Pool {
    fn content_store(&self) -> ContentStore {
        self.content.clone()
    }

    async fn put_publication(
        &self,
        _objects: &[(ObjectId, Vec<u8>)],
        _content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        Err(ObjectStoreError::Backend("the bench's pool is only collected".into()).into())
    }

    fn collectable_structure(&self) -> Option<&dyn CollectableStructure> {
        Some(&self.objects.inner)
    }
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
