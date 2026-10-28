//! Importing a network from a bundle.
//!
//! `RepositoriesDirectory::import_network` copies the bundle's structure into a stage
//! (hashing it against the manifest), adds the content the heads reach to the
//! directory's content store, validates every distinct state and installs the network.
//! The case builds and exports the network `network_export` does, then imports it into
//! an empty directory and records the wall time and what the target then holds. The
//! imported network must hold the bundle's content, once each.

use std::{path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::ContentId;
use wheat_store::{
    PackedObjectStore, RepositoriesDirectory, read_bundle_content_chunk, read_bundle_manifest,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    network_export::{Built, build_network},
};

pub(crate) const NAME: &str = "network_import";
/// The history size the budgets are checked at.
const LARGEST: usize = 10_000;
/// Measured on 2026-10-28 (release, NVMe): about 21 s, nearly all of it
/// `validate_complete` re-verifying the 10,000-change history, as in `network_export`;
/// copying the structure and adding the content is a second or two.
const IMPORT_BUDGET_SECONDS: f64 = 45.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(LARGEST, 30);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_import(&context.scratch, changes, &mut recorder))?;

    recorder.at_most("import.wall", IMPORT_BUDGET_SECONDS, "s");
    Ok(recorder.finish())
}

async fn measure_import(scratch: &Path, changes: usize, recorder: &mut Recorder) -> Result<()> {
    let Built {
        repositories: source,
        location,
    } = build_network(&scratch.join("source"), changes).await?;
    let network = location.network;
    let bundle = scratch.join("bundle");
    let store = PackedObjectStore::open(source.network(network).storage())?;
    let held = store.content_store();
    source
        .export_network(network, &bundle)?
        .finish(&held)
        .await?;
    drop(held);
    store.close()?;
    let manifest = read_bundle_manifest(&bundle)?;
    let repositories = source
        .network(network)
        .repositories()?
        .into_iter()
        .zip(["target/one", "target/two"])
        .map(|(repository, name)| (repository, name.to_owned()))
        .collect::<Vec<_>>();

    let target = RepositoriesDirectory::new(scratch.join("target"));
    std::fs::create_dir(target.root())?;
    let content = target.open_content()?;
    let held = content.content_store();
    let started = Instant::now();
    let imported = target.import_network(&bundle, &repositories, &held).await?;
    recorder.wall("import", started.elapsed().as_secs_f64());

    ensure!(imported.network == network, "the network's ID changed");
    ensure!(!imported.resumed, "the network was already there");
    for (name, placed) in &imported.repositories {
        ensure!(
            target.locate(name)? == Some(placed.location),
            "{name} does not name its repository"
        );
    }
    // Every content the bundle lists is in the target's store.
    for index in 0..manifest.content_chunks.len() {
        let ids = read_bundle_content_chunk(&bundle, &manifest, index)?
            .into_iter()
            .map(|entry| ContentId::from_digest(entry.id))
            .collect::<Vec<_>>();
        ensure!(
            held.existing_ids(&ids)?.len() == ids.len(),
            "chunk {index} of the content list is not all in the target"
        );
    }
    recorder.value("content_count", manifest.content_count as f64, "count");
    recorder.value("content_bytes", manifest.content_bytes as f64, "bytes");
    drop(held);
    content.close()?;
    Ok(())
}
