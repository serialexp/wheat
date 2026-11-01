//! Opening a packed store whose content lives in a content generation.
//!
//! Writes `blobs` distinct 4 KiB blobs, repacks them into one content generation and
//! ages its files, then opens the store twice: the first open reads the generation
//! whole and records it, the second finds its files unchanged and skips the read.
//! Measured only; `packed_repack::an_unchanged_content_generation_is_read_whole_once`
//! checks the skip.

use std::{
    fs::File,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, ensure};
use wheat_core::{Repository, Tree, VerifiedContent};
use wheat_store::{LocalPublicationStore, PackedObjectStore, RepackOptions, RepositoryPublisher};

use super::{CaseContext, CaseOutput, Recorder};

pub(crate) const NAME: &str = "packed_open";
const BLOB_BYTES: usize = 4096;
const BATCH: usize = 1024;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let blobs = context.scale.pick(50_000, 200);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("blobs", blobs);
    recorder.parameter("blob_bytes", BLOB_BYTES);

    let root = tempfile::tempdir_in(&context.scratch)?;
    let storage = root.path().join("storage");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let objects = PackedObjectStore::open(&storage)?;
    let content = objects.content_store();
    let mut batch = Vec::with_capacity(BATCH);
    for blob in 0..blobs {
        batch.push(VerifiedContent::new(noise(blob as u64)));
        if batch.len() == BATCH || blob + 1 == blobs {
            let borrowed = batch
                .iter()
                .map(VerifiedContent::borrowed)
                .collect::<Vec<_>>();
            content.put_many(&borrowed, 1)?;
            batch.clear();
        }
    }
    let heads = LocalPublicationStore::new(root.path().join("repository.head"));
    runtime.block_on(async {
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&Repository::new(Tree::new()))
            .await?;
        let plan = objects
            .plan_repack(&heads, RepackOptions::default())
            .await?;
        objects.execute_repack(&heads, plan)?;
        anyhow::Ok(())
    })?;
    drop(content);
    objects.close()?;

    // A file changed in the last two seconds is read whole at every open.
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    let mut pack_bytes = 0;
    for generation in std::fs::read_dir(storage.join("content-generations"))? {
        let generation = generation?;
        if !generation.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(generation.path())? {
            let path = file?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pack")
            {
                pack_bytes += std::fs::metadata(&path)?.len();
            }
            File::options()
                .write(true)
                .open(&path)?
                .set_modified(an_hour_ago)?;
        }
    }
    ensure!(pack_bytes > 0, "the repack wrote no content generation");
    recorder.value("pack_bytes", pack_bytes as f64, "bytes");

    for step in ["first_open", "checked_open"] {
        let started = Instant::now();
        let objects = PackedObjectStore::open(&storage)?;
        recorder.wall(step, started.elapsed().as_secs_f64());
        objects.close()?;
    }
    ensure!(
        storage.join("content-generations/checked").is_file(),
        "the first open recorded no check"
    );
    Ok(recorder.finish())
}

/// [`BLOB_BYTES`] of xorshift noise seeded with `seed`, which compress poorly, so
/// the pack holds about as many bytes as were written.
fn noise(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut bytes = Vec::with_capacity(BLOB_BYTES);
    while bytes.len() < BLOB_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes
}
