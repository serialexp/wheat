//! Validating a one-change publication as the history behind it grows.
//!
//! Changes are created on the frontier and published one at a time, the way
//! `wheat change new` does, with their files spread over two levels of directories.
//! At each checkpoint the last publication is validated as an increment on the one
//! before: the first run (nothing of it read yet in this process) and the median of
//! [`SAMPLES`] more, with the objects and bytes the validator read. The mean
//! create-and-publish time of the [`CREATE_WINDOW`] changes up to each checkpoint is
//! recorded too, and the slowest single one since the checkpoint before, which shows
//! store work that only some publications pay.
//!
//! The validator reads a root-to-leaf path in each collection a change touches and
//! walks the order index's skip chain, so its reads grow with the logarithm of the
//! history. What matters is the wall time: a one-change publication after
//! [`LARGEST`] changes validates within [`VALIDATE_BUDGET_SECONDS`].

use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use anyhow::Result;
use async_trait::async_trait;
use wheat_core::{ChangeId, ChangeMetadata, ChangeProposal, ObjectId, RevisionMetadata};
use wheat_store::{
    ImmutableObjectStore, LocalPublicationStore, ObjectStoreError, PackedObjectStore,
    PersistedRepositoryMutator, RepositoryReader, StoredObject,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Scratch, author, rewrite},
};
use crate::measure;

pub(crate) const NAME: &str = "increment_validate";
/// Warm validations after the first, per checkpoint.
const SAMPLES: usize = 5;
/// Creates before each checkpoint whose mean time is recorded.
const CREATE_WINDOW: usize = 100;
/// The history size the budget is checked at.
const LARGEST: usize = 100_000;
const VALIDATE_BUDGET_SECONDS: f64 = 0.1;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 10_000, LARGEST], vec![CREATE_WINDOW, 300]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_history(
        &context.scratch,
        &checkpoints,
        &mut recorder,
    ))?;
    recorder.at_most(
        &format!("n{LARGEST}.validate.wall"),
        VALIDATE_BUDGET_SECONDS,
        "s",
    );
    Ok(recorder.finish())
}

/// An object store counting the objects and bytes read through it.
struct Counting {
    inner: PackedObjectStore,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    /// The reads and bytes since the last call.
    fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }
}

#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let object = self.inner.get(id).await?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(object.canonical_bytes.len() as u64, Ordering::Relaxed);
        Ok(object)
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.inner.seal().await
    }
}

async fn measure_history(
    root: &Path,
    checkpoints: &[usize],
    recorder: &mut Recorder,
) -> Result<()> {
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting {
        inner: scratch.objects,
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    };
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let reader = RepositoryReader::new(&objects, &scratch.content);
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut effective = base;
    let mut creates = VecDeque::with_capacity(CREATE_WINDOW);
    let mut slowest = 0.0_f64;
    let started = Instant::now();
    for index in 0..largest {
        let created = Instant::now();
        let path = format!(
            "d{:02x}/d{:02x}/f{index}.txt",
            index & 0xff,
            (index >> 8) & 0xff
        );
        let (_, edits) = rewrite(&scratch.content, &path, path.as_bytes(), None)?;
        let (prepared, outcome) = mutator
            .create_on_frontier(
                head,
                base,
                effective,
                ChangeProposal {
                    id: ChangeId::new(),
                    metadata: ChangeMetadata {
                        title: format!("Change {index}"),
                        creator: author(),
                        ..ChangeMetadata::default()
                    },
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata {
                        author: author(),
                        committer: author(),
                        ..RevisionMetadata::default()
                    },
                    edits,
                },
            )
            .await?;
        let previous = head.envelope;
        head = mutator.publish(prepared).await?;
        effective = outcome.resulting_view;
        if creates.len() == CREATE_WINDOW {
            creates.pop_front();
        }
        let create = created.elapsed().as_secs_f64();
        creates.push_back(create);
        slowest = slowest.max(create);
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let step = format!("n{size}");
        recorder.wall(&format!("{step}.build"), started.elapsed().as_secs_f64());
        recorder.wall(
            &format!("{step}.create_mean"),
            creates.iter().sum::<f64>() / creates.len() as f64,
        );
        recorder.wall(&format!("{step}.create_max"), slowest);
        slowest = 0.0;

        objects.take();
        let first = Instant::now();
        reader.validate_increment(previous, head.envelope).await?;
        recorder.wall(
            &format!("{step}.validate_first"),
            first.elapsed().as_secs_f64(),
        );
        let (reads, bytes) = objects.take();
        recorder.value(&format!("{step}.validate.reads"), reads as f64, "count");
        recorder.value(&format!("{step}.validate.bytes"), bytes as f64, "bytes");
        let mut walls = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let sample = Instant::now();
            reader.validate_increment(previous, head.envelope).await?;
            walls.push(sample.elapsed().as_secs_f64());
        }
        objects.take();
        recorder.wall(&format!("{step}.validate"), measure::median(walls));
        eprintln!(
            "{NAME}: {size} changes after {:.0} s",
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
