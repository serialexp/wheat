//! Forking a served repository, as a host's fork button does.
//!
//! `wheat_store::RepositoriesDirectory::fork` gives the fork a head in its parent's
//! fork network naming the parent's envelope, so it reads one head and writes one
//! head and one name, whatever the size of the history. The case builds one
//! repository in a network, changes created on the frontier and published one at a
//! time, and at each checkpoint forks it [`FORKS`] times, recording the median wall
//! time of a fork and the bytes the forks added to the network's store. At
//! [`LARGEST`] changes a fork may take [`FORK_BUDGET_SECONDS`] and add nothing to the
//! store.

use std::{path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{Repository, Tree};
use wheat_store::{
    Durability, LocalPublicationStore, NetworkDirectory, PackedObjectStore, PublicationStore,
    RepositoriesDirectory,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Layout, Scratch, create_at},
};
use crate::measure;

pub(crate) const NAME: &str = "fork_network";
/// Forks made at each checkpoint.
const FORKS: usize = 20;
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
/// A fork syncs a few small files.
const FORK_BUDGET_SECONDS: f64 = 0.05;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(vec![1_000, LARGEST], vec![20, 60]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("forks", FORKS);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_forks(
        &context.scratch.join("repositories"),
        &checkpoints,
        &mut recorder,
    ))?;

    let step = format!("n{LARGEST}");
    recorder.at_most(&format!("{step}.fork.wall"), FORK_BUDGET_SECONDS, "s");
    recorder.at_most(&format!("{step}.store_bytes_added"), 0.0, "bytes");
    Ok(recorder.finish())
}

async fn measure_forks(root: &Path, checkpoints: &[usize], recorder: &mut Recorder) -> Result<()> {
    let repositories = RepositoriesDirectory::new(root);
    let placed = repositories.create_empty("owner/repo", "main").await?;
    let network = repositories.network(placed.location.network);
    let base = Repository::new(Tree::new()).base_view().id;
    let mut head = placed.head;
    let mut effective = base;
    let mut built = 0;
    for &changes in checkpoints {
        let objects = PackedObjectStore::open_deferred(network.storage())?;
        let scratch = Scratch {
            content: objects.content_store(),
            objects,
            heads: LocalPublicationStore::with_durability(
                network.head(placed.location.repository),
                Durability::Deferred,
            ),
        };
        let mutator = scratch.mutator();
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
        // Closing a deferred store publishes what it wrote ahead into its segments.
        let Scratch {
            objects, content, ..
        } = scratch;
        drop(content);
        objects.close()?;

        let step = format!("n{changes}");
        let before = store_bytes(&network)?;
        let mut walls = Vec::with_capacity(FORKS);
        for fork in 0..FORKS {
            let started = Instant::now();
            let forked = repositories.fork("owner/repo", &format!("fork{fork}/n{changes}"))?;
            walls.push(started.elapsed().as_secs_f64());
            ensure!(
                forked.head.envelope == head.envelope,
                "the fork names another envelope"
            );
        }
        let added = store_bytes(&network)?.saturating_sub(before);
        recorder.wall(&format!("{step}.fork"), measure::median(walls));
        recorder.value(&format!("{step}.store_bytes_added"), added as f64, "bytes");
        recorder.value(
            &format!("{step}.repositories"),
            network.repositories()?.len() as f64,
            "count",
        );
        ensure!(
            network.publications(placed.location.repository).read()? == head,
            "the parent's head moved"
        );
    }
    Ok(())
}

/// The bytes of the network's store.
fn store_bytes(network: &NetworkDirectory) -> Result<u64> {
    Ok(network
        .files()?
        .iter()
        .filter(|file| file.path.starts_with("storage"))
        .map(|file| file.len)
        .sum())
}
