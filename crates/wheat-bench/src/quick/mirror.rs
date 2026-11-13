//! Mirroring a Wheat remote into a served repository (`wheat_server::mirror`), one
//! change at a time, on a long history.
//!
//! The remote is a server in this process over one linear history, change `i`
//! writing one file, built a checkpoint at a time. The mirror catches up at each
//! checkpoint; then the remote gains one change, and the mirror takes it. That one
//! mirror is counted in objects read from the mirror's store: the fetch's own walk,
//! the naming of what the new head relies on, and the validation of what arrived,
//! which is only of what it adds since the remote's history extends the mirror's
//! head. Each grows with the history only as the collections deepen, with its
//! logarithm: from the shortest history to the longest, the reads grow by at most
//! [`READ_GROWTH_PER_BIT`] percent of the shortest's per doubling of the history. A
//! mirror that validated the whole repository each time would grow with it.

use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration, time::Instant};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use wheat_core::{ContentId, ObjectId};
use wheat_server::{
    access::Principal,
    mirror::mirror,
    naming::ContentNaming,
    network::Network,
    registry::{Registry, RepositoryEntry},
    server::{ServerConfig, run_until},
};
use wheat_store::{
    ImmutableObjectStore, ObjectStoreError, PublicationStore, SeparateRepositoryStore, StoredObject,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, create_at},
};

pub(crate) const NAME: &str = "mirror";
/// The history sizes measured; the budget compares the last with the first.
const CHECKPOINTS: [usize; 2] = [1_000, 10_000];
/// Most one change's mirror may grow in reads per doubling of the history, in
/// percent of what it read at the shortest. Measured 2026-11-13 (release) at 1,000
/// and 10,000 changes: 126 and 149 reads, 5.5% per doubling. Validating the whole
/// repository on each mirror read 19,455 and 309,612, 449% per doubling. The
/// collections gain levels in steps, so the budget leaves room for one more.
const READ_GROWTH_PER_BIT: f64 = 15.0;
const REMOTE: &str = "bench/remote";

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(CHECKPOINTS.to_vec(), vec![20, 60]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let reads = runtime.block_on(measure(
        &context.scratch.join("mirror"),
        &checkpoints,
        &mut recorder,
    ))?;

    let (first, last) = (reads[0], reads[reads.len() - 1]);
    let bits = (checkpoints[checkpoints.len() - 1] as f64 / checkpoints[0] as f64).log2();
    let name = "one_change.read_growth_per_bit";
    let growth = (last as f64 / first as f64 - 1.0) / bits * 100.0;
    recorder.value(name, growth, "percent");
    recorder.at_most(name, READ_GROWTH_PER_BIT, "percent");
    Ok(recorder.finish())
}

/// The counting store, shared with the mirror's network.
struct Shared(Arc<Counting>);

#[async_trait]
impl ImmutableObjectStore for Shared {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.0.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.0.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.0.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.0.seal().await
    }
}

/// A network told what publishes rely on, as Gothab's is, that stamps nothing: the
/// naming's reads are counted, not its stamps.
struct Unstamped;

#[async_trait]
impl ContentNaming for Unstamped {
    fn renew_after(&self) -> Duration {
        Duration::from_secs(3600)
    }

    async fn named(&self, _ids: &[ContentId]) -> io::Result<()> {
        Ok(())
    }
}

/// Builds the remote's history and, at each checkpoint, mirrors one new change:
/// what each of those mirrors read.
async fn measure(root: &Path, checkpoints: &[usize], recorder: &mut Recorder) -> Result<Vec<u64>> {
    std::fs::create_dir_all(root)?;
    let remote = Scratch::open(&root.join("remote"))?;
    let (base, mut head) = remote.initialize().await?;
    let registry = Arc::new(Registry::single_entry(
        REMOTE.to_string(),
        RepositoryEntry::in_network(
            Network::new(Box::new(SeparateRepositoryStore::new(
                remote.objects.clone(),
                remote.content.clone(),
            ))),
            remote.heads.clone(),
        ),
    ));
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let any_port: SocketAddr = "127.0.0.1:0".parse()?;
    let server = tokio::spawn(run_until(
        any_port,
        registry,
        ServerConfig::new(),
        ready,
        shutdown_rx,
    ));
    let url = wheat_remote::parse_wheat_url(&format!("wheat://{}/{REMOTE}", ready_rx.await?))?;

    let local = Scratch::open(&root.join("mirror"))?;
    local.initialize().await?;
    let counting = Arc::new(Counting::new(local.objects.clone()));
    let entry = Arc::new(RepositoryEntry::in_network(
        Network::with_naming(
            Box::new(SeparateRepositoryStore::new(
                Shared(Arc::clone(&counting)),
                local.content.clone(),
            )),
            Arc::new(Unstamped),
        ),
        local.heads.clone(),
    ));
    let principal = Principal {
        name: "wheat-bench".into(),
    };
    let mutator = remote.mutator();
    let mut effective = base;
    let mut built = 0;
    let mut reads = Vec::with_capacity(checkpoints.len());
    for &changes in checkpoints {
        for index in built..=changes {
            if index == changes {
                // The history up to the checkpoint is mirrored before its last change.
                mirror(&entry, &principal, &url, b"")
                    .await
                    .map_err(|e| anyhow::anyhow!("catch up: {e:?}"))?;
            }
            (head, effective) = create_at(
                &mutator,
                &remote.content,
                head,
                base,
                effective,
                index,
                Layout::Spread,
            )
            .await?;
        }
        built = changes + 1;
        let step = format!("n{changes}");

        counting.take();
        let started = Instant::now();
        let mirrored = mirror(&entry, &principal, &url, b"")
            .await
            .map_err(|e| anyhow::anyhow!("mirror one change: {e:?}"))?;
        recorder.wall(
            &format!("{step}.one_change"),
            started.elapsed().as_secs_f64(),
        );
        let (read, _) = counting.take();
        ensure!(mirrored.moved, "the remote's change was not mirrored");
        ensure!(
            mirrored.head.envelope
                == remote
                    .heads
                    .read()
                    .context("read the remote's head")?
                    .envelope,
            "the mirror is not at the remote's head"
        );
        ensure!(
            mirrored.objects.incremental,
            "the fetch was not incremental"
        );
        recorder.value(&format!("{step}.one_change.reads"), read as f64, "count");
        recorder.value(
            &format!("{step}.one_change.objects"),
            (mirrored.objects.pushed + mirrored.objects.pulled) as f64,
            "count",
        );
        recorder.value(
            &format!("{step}.one_change.content_fetched"),
            mirrored.content.fetched as f64,
            "count",
        );
        reads.push(read);
    }
    shutdown.send(()).ok();
    server.await??;
    std::fs::remove_dir_all(root)?;
    Ok(reads)
}
