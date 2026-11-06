//! An embedder's edit and merge, built in a `wheat_server::write::ServerStage` and
//! submitted with a label change, on a served repository with a long history.
//!
//! One linear history, change `i` adding one file, is built a checkpoint at a time.
//! At each checkpoint, counted in objects the server reads from its store:
//!
//! - `edit`: a stage creating one change on the history's view, submitted creating
//!   a label at it;
//! - `merge`: two such changes submitted under labels of their own, then a stage
//!   integrating both into one view, submitted creating a label at it.
//!
//! The mutator reads the collections down to the change it composes, the submit
//! compares the built state with the base's and validates the transition, and the
//! merge reads the two changes it joins, so what each reads grows with the history only
//! as the collections deepen, with its logarithm: from the shortest history to the
//! longest, each grows by at most [`READ_GROWTH_PER_BIT`] percent of the shortest's per
//! doubling of the history.

use std::{path::Path, sync::Arc, time::Instant};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use wheat_core::{ObjectId, ViewId, ViewLabel};
use wheat_server::{
    access::Principal,
    network::Network,
    registry::RepositoryEntry,
    write::{ExpectedLabel, LabelChange, ServerStage, ServerWrite, open_stage},
};
use wheat_store::{
    ImmutableObjectStore, ObjectStoreError, PublicationStore, SeparateRepositoryStore, StoredObject,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, change_id, committed_at, create_at, rewrite},
};

pub(crate) const NAME: &str = "server_stage";
/// The history sizes measured; the budgets compare the last with the first.
const CHECKPOINTS: [usize; 2] = [1_000, 10_000];
/// Most an operation's reads may grow per doubling of the history, in percent of what
/// it read at the shortest. Measured 2026-11-07 (release) at 1,000 and 10,000 changes:
/// edit 108 and 125 reads, merge 122 and 147, so 4.7% and 6.2% per doubling, against
/// 100% if they grew with the history. The collections gain levels in steps, so the
/// budget leaves room for one more.
const READ_GROWTH_PER_BIT: f64 = 15.0;
const STEPS: [&str; 2] = ["edit", "merge"];

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(CHECKPOINTS.to_vec(), vec![20, 60]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let reads = runtime.block_on(measure(
        &context.scratch.join("stage"),
        &checkpoints,
        &mut recorder,
    ))?;

    let (first, last) = (&reads[0], &reads[reads.len() - 1]);
    let bits = (checkpoints[checkpoints.len() - 1] as f64 / checkpoints[0] as f64).log2();
    for (step, (small, large)) in STEPS.iter().zip(first.iter().zip(last)) {
        let name = format!("{step}.read_growth_per_bit");
        let growth = (*large as f64 / *small as f64 - 1.0) / bits * 100.0;
        recorder.value(&name, growth, "percent");
        recorder.at_most(&name, READ_GROWTH_PER_BIT, "percent");
    }
    Ok(recorder.finish())
}

/// The counting store, shared with the server's network.
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

fn label_write(key: usize, name: &str, view: ViewId) -> ServerWrite {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&(key as u64).to_be_bytes());
    ServerWrite {
        principal: Principal {
            name: "wheat-bench".into(),
        },
        key: bytes,
        labels: vec![LabelChange {
            name: name.to_owned(),
            expected: ExpectedLabel::Any,
            new: Some(ViewLabel::from(view)),
        }],
        force_labels: false,
    }
}

/// What the submit under key `key` asks for.
fn intent(key: usize) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&(key as u64).to_be_bytes());
    bytes
}

/// Builds, in `stage`, change number `index` adding one file on `view`; answers the
/// view after it.
async fn edit_in(stage: &ServerStage, view: ViewId, index: usize) -> Result<ViewId> {
    stage
        .build(move |builder| async move {
            let path = format!("stage/f{index}.txt");
            let (_, edits) = rewrite(builder.content(), &path, path.as_bytes(), None)?;
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .create_on_frontier(
                    builder.head(),
                    view,
                    view,
                    wheat_core::ChangeProposal {
                        id: change_id(index),
                        metadata: wheat_core::ChangeMetadata::default(),
                        dependencies: Default::default(),
                        revision_metadata: wheat_core::RevisionMetadata {
                            committed_at: committed_at(index),
                            ..wheat_core::RevisionMetadata::default()
                        },
                        edits,
                    },
                )
                .await?;
            mutator.publish(prepared).await?;
            Ok::<_, anyhow::Error>(outcome.resulting_view)
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
}

/// Builds the history and, at each checkpoint, edits and merges through stages:
/// what each read, in [`STEPS`] order.
async fn measure(
    root: &Path,
    checkpoints: &[usize],
    recorder: &mut Recorder,
) -> Result<Vec<[u64; 2]>> {
    std::fs::create_dir_all(root)?;
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let counting = Arc::new(Counting::new(scratch.objects.clone()));
    let store =
        SeparateRepositoryStore::new(Shared(Arc::clone(&counting)), scratch.content.clone());
    let entry = Arc::new(RepositoryEntry::in_network(
        Network::new(Box::new(store)),
        scratch.heads.clone(),
    ));
    let mutator = scratch.mutator();
    let mut effective = base;
    let mut built = 0;
    // Stage changes are numbered past every history change.
    let mut extra = checkpoints[checkpoints.len() - 1];
    let mut reads = Vec::with_capacity(checkpoints.len());
    for &changes in checkpoints {
        for index in built..changes {
            (head, effective) = create_at(
                &mutator,
                &scratch.content,
                head,
                base,
                effective,
                index,
                Layout::Spread,
            )
            .await?;
        }
        built = changes;
        let step = format!("n{changes}");

        counting.take();
        let started = Instant::now();
        let stage = open_stage(&entry)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let edited = edit_in(&stage, effective, extra).await?;
        stage
            .submit(
                label_write(extra, &format!("{step}/edited"), edited),
                intent(extra),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        recorder.wall(&format!("{step}.edit"), started.elapsed().as_secs_f64());
        let edit = counting.take().0;
        recorder.value(&format!("{step}.edit.reads"), edit as f64, "count");
        extra += 1;

        let mut sides = Vec::new();
        for name in ["a", "b"] {
            let stage = open_stage(&entry)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let side = edit_in(&stage, effective, extra).await?;
            stage
                .submit(
                    label_write(extra, &format!("{step}/{name}"), side),
                    intent(extra),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            sides.push(side);
            extra += 1;
        }
        counting.take();
        let started = Instant::now();
        let stage = open_stage(&entry)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let merged = stage
            .build(move |builder| async move {
                let mutator = builder.mutator();
                let (prepared, outcome) =
                    mutator.integrate(builder.head(), effective, &sides).await?;
                anyhow::ensure!(outcome.conflicted.is_empty(), "the merge conflicted");
                mutator.publish(prepared).await?;
                Ok(outcome.resulting_view)
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))??;
        stage
            .submit(
                label_write(extra, &format!("{step}/merged"), merged),
                intent(extra),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        recorder.wall(&format!("{step}.merge"), started.elapsed().as_secs_f64());
        let merge = counting.take().0;
        recorder.value(&format!("{step}.merge.reads"), merge as f64, "count");
        extra += 1;

        // The history goes on from the head the stages published.
        head = scratch.heads.read().context("read the head")?;
        reads.push([edit, merge]);
    }
    std::fs::remove_dir_all(root)?;
    Ok(reads)
}
