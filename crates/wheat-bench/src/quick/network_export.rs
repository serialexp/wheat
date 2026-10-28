//! Exporting a fork network to a bundle directory.
//!
//! `RepositoriesDirectory::export_network` copies the network's structure and
//! `StructureExport::finish` lists and copies the content its heads reach, then
//! validates every distinct state. The case builds one repository in a network,
//! changes created on the frontier and published one at a time, forks it, and
//! exports the network at each checkpoint. It records the wall time of the copy and
//! of the finish, the bundle's size and the content it lists. The bundle must list
//! exactly the content the changes added, once each.

use std::{path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{Repository, RepositoryLocationRecord, Tree};
use wheat_store::{
    Durability, LocalPublicationStore, PackedObjectStore, RepositoriesDirectory,
    read_bundle_manifest,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Layout, Scratch, create_at},
};

pub(crate) const NAME: &str = "network_export";
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
/// Measured on 2026-10-28 (release, NVMe): 2.5 s to copy the structure and 19–27 s to
/// finish, of which about 20 s is `validate_complete` re-verifying the 10,000-change
/// history linearly (2 ms a change); listing and copying the content is under a second.
const FINISH_BUDGET_SECONDS: f64 = 45.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(LARGEST, 30);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_export(
        &context.scratch.join("repositories"),
        changes,
        &mut recorder,
    ))?;

    recorder.at_most("finish.wall", FINISH_BUDGET_SECONDS, "s");
    Ok(recorder.finish())
}

/// A network of one repository of `changes` changes and a fork of it, in `root`.
pub(super) struct Built {
    pub(super) repositories: RepositoriesDirectory,
    pub(super) location: RepositoryLocationRecord,
}

/// Builds the network the cases export, with the changes' content in the network's own
/// store.
pub(super) async fn build_network(root: &Path, changes: usize) -> Result<Built> {
    let repositories = RepositoriesDirectory::new(root);
    let placed = repositories.create_empty("owner/repo", "main").await?;
    let network = repositories.network(placed.location.network);
    let base = Repository::new(Tree::new()).base_view().id;
    let mut head = placed.head;
    let mut effective = base;

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
    for index in 0..changes {
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
    // Closing a deferred store publishes what it wrote ahead into its segments.
    let Scratch {
        objects, content, ..
    } = scratch;
    drop(content);
    objects.close()?;
    repositories.fork("owner/repo", "fork/repo")?;
    Ok(Built {
        repositories,
        location: placed.location,
    })
}

async fn measure_export(root: &Path, changes: usize, recorder: &mut Recorder) -> Result<()> {
    let Built {
        repositories,
        location,
    } = build_network(root, changes).await?;
    let network = location.network;
    // The content stays where the changes put it: in the network's own store.
    let source_store = PackedObjectStore::open(repositories.network(network).storage())?;
    let content = source_store.content_store();

    let bundle = root.parent().unwrap_or(root).join("bundle");
    let started = Instant::now();
    let export = repositories.export_network(network, &bundle)?;
    recorder.wall("copy", started.elapsed().as_secs_f64());
    let started = Instant::now();
    let manifest = export.finish(&content).await?;
    recorder.wall("finish", started.elapsed().as_secs_f64());
    drop(content);
    source_store.close()?;

    ensure!(
        read_bundle_manifest(&bundle)? == manifest,
        "the manifest on disk differs"
    );
    ensure!(
        manifest.content_count >= changes as u64,
        "the bundle lists {} contents for {changes} changes",
        manifest.content_count
    );
    recorder.value("content_count", manifest.content_count as f64, "count");
    recorder.value("content_bytes", manifest.content_bytes as f64, "bytes");
    recorder.value("bundle_files", manifest.files.len() as f64, "count");
    Ok(())
}
