//! Fresh-store, phase-separated profiling; leaves artifacts for inspection.
use async_trait::async_trait;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use wheat_core::ObjectId;
use wheat_git::{FinalizePhase, finalize_import_with_observer, stage_import};
use wheat_store::*;

const OBJECT_KIND_COUNT: usize = 16;

struct Meter {
    inner: PackedObjectStore,
    reads: AtomicU64,
    writes: AtomicU64,
    bytes: AtomicU64,
    reads_by_kind: [AtomicU64; OBJECT_KIND_COUNT],
}
#[async_trait]
impl ImmutableObjectStore for Meter {
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let object = self.inner.get(id).await?;
        self.reads_by_kind[object.kind as usize - 1].fetch_add(1, Ordering::Relaxed);
        Ok(object)
    }
    async fn put_if_absent(&self, id: ObjectId, b: &[u8]) -> Result<(), ObjectStoreError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        self.inner.put_if_absent(id, b).await
    }
    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.writes
            .fetch_add(objects.len() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(
            objects.iter().map(|(_, b)| b.len() as u64).sum::<u64>(),
            Ordering::Relaxed,
        );
        self.inner.put_batch(objects, concurrency).await
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}
impl Meter {
    fn report(&self, phase: &str, start: Instant) {
        println!(
            "phase={phase} seconds={:.6} object_gets={} object_put_attempts={} submitted_bytes={}",
            start.elapsed().as_secs_f64(),
            self.reads.swap(0, Ordering::Relaxed),
            self.writes.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed)
        );
        println!(
            "phase={phase} reads_by_kind={:?}",
            self.reads_by_kind
                .iter()
                .map(|value| value.swap(0, Ordering::Relaxed))
                .collect::<Vec<_>>()
        );
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("source path required")?);
    let output = PathBuf::from(args.next().ok_or("fresh output path required")?);
    let branch = args
        .next()
        .ok_or("full working ref required")?
        .into_string()
        .map_err(|_| "branch must be UTF-8")?;
    std::fs::create_dir(&output)?;
    println!(
        "source={} storage={} branch={branch}",
        source.display(),
        output.display()
    );
    let objects = Meter {
        inner: PackedObjectStore::open(output.join("storage"))?,
        reads: AtomicU64::new(0),
        writes: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        reads_by_kind: std::array::from_fn(|_| AtomicU64::new(0)),
    };
    let content = objects.inner.content_store();
    let heads = LocalPublicationStore::new(output.join("repository.head"));
    let journal = output.join("import.sqlite");
    let start = Instant::now();
    println!("starting=stage");
    let staged = stage_import(&source, &journal, &objects, &content, None).await?;
    objects.report("stage", start);
    println!(
        "commits={} branches={} complete={}",
        staged.completed_commits,
        staged.branches.len(),
        staged.complete
    );
    let start = Instant::now();
    println!("starting=resume_verify");
    let resumed = stage_import(&source, &journal, &objects, &content, Some(0)).await?;
    assert_eq!(staged, resumed);
    objects.report("resume_verify", start);
    let phase_start = std::cell::Cell::new(None::<Instant>);
    let last_phase = std::cell::Cell::new(None::<FinalizePhase>);
    let head = finalize_import_with_observer(
        &source,
        &journal,
        &objects,
        &content,
        &heads,
        &branch,
        |phase| {
            if let (Some(previous), Some(start)) = (last_phase.get(), phase_start.get()) {
                objects.report(previous.as_str(), start);
            }
            println!("starting={}", phase.as_str());
            last_phase.set(Some(phase));
            phase_start.set(Some(Instant::now()));
        },
    )
    .await?;
    if let (Some(phase), Some(start)) = (last_phase.get(), phase_start.get()) {
        objects.report(phase.as_str(), start);
    }
    println!("generation={} envelope={}", head.generation, head.envelope);
    Ok(())
}
