//! Encoding and decoding a sharded content-generation manifest (formerly the ignored
//! `content_repack::benchmark_sharded_manifest_208k_entries` test).
//!
//! 208,000 entries is the size of the Gothab repository's content generation. Each
//! step runs three times and records the median. Measured only.

use std::time::Instant;

use anyhow::{Context, Result, ensure};
use wheat_core::ContentId;
use wheat_store::{
    CONTENT_GENERATION_SHARD_ENTRIES, ContentGenerationEntry, ContentGenerationIndex,
    ContentGenerationManifest,
};

use super::{CaseContext, CaseOutput, Recorder};
use crate::measure;

pub(crate) const NAME: &str = "sharded_manifest";
const SAMPLES: usize = 3;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    // Smoke still spans three shards, so the middle one is full.
    let count = context.scale.pick(208_000, 40_000);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("entries", count);
    recorder.parameter("samples", SAMPLES);

    let (mut seal, mut encode, mut decode, mut one_shard) = (
        Vec::with_capacity(SAMPLES),
        Vec::with_capacity(SAMPLES),
        Vec::with_capacity(SAMPLES),
        Vec::with_capacity(SAMPLES),
    );
    let mut sizes = (0, 0, 0);
    for _ in 0..SAMPLES {
        let entries = entries(count)?;
        let started = Instant::now();
        let manifest = ContentGenerationManifest::new(1, entries)?;
        seal.push(started.elapsed().as_secs_f64());

        let started = Instant::now();
        let index = manifest.index()?.encoded()?;
        let shards = (0..manifest.shard_count())
            .map(|shard| manifest.encode_shard(shard))
            .collect::<Result<Vec<_>, _>>()?;
        encode.push(started.elapsed().as_secs_f64());

        let started = Instant::now();
        let decoded = ContentGenerationManifest::decode(&index, |shard| Ok(shards[shard].clone()))?;
        decode.push(started.elapsed().as_secs_f64());

        let middle = shards.len() / 2;
        let started = Instant::now();
        let shard =
            ContentGenerationIndex::decode(&index)?.decode_shard(middle, &shards[middle])?;
        one_shard.push(started.elapsed().as_secs_f64());

        ensure!(decoded == manifest, "decoded manifest differs");
        ensure!(
            shard.len() == CONTENT_GENERATION_SHARD_ENTRIES,
            "the middle shard is not full"
        );
        sizes = (
            index.len(),
            shards.len(),
            shards.iter().map(Vec::len).sum::<usize>(),
        );
    }

    for (step, samples) in [
        ("seal", seal),
        ("encode", encode),
        ("decode", decode),
        ("index_and_one_shard", one_shard),
    ] {
        recorder.wall(step, measure::median(samples));
    }
    let (index_bytes, shard_count, shard_bytes) = sizes;
    recorder.value("index_bytes", index_bytes as f64, "bytes");
    recorder.value("shards", shard_count as f64, "count");
    recorder.value("shard_bytes", shard_bytes as f64, "bytes");
    Ok(recorder.finish())
}

fn entries(count: usize) -> Result<Vec<ContentGenerationEntry>> {
    (0..count)
        .map(|index| {
            let digest = |value: u64| -> Result<[u8; 32]> {
                value
                    .to_be_bytes()
                    .repeat(4)
                    .try_into()
                    .ok()
                    .context("digest is 32 bytes")
            };
            Ok(ContentGenerationEntry {
                id: ContentId::from_digest(digest(index as u64)?),
                envelope_digest: digest((index as u64).wrapping_mul(3))?,
                pack_offset: (index % CONTENT_GENERATION_SHARD_ENTRIES) as u64 * 128,
                physical_length: 128,
                logical_length: 256,
                delta_base: None,
                delta_depth: 0,
            })
        })
        .collect()
}
