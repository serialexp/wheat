//! Opt-in, read-only benchmark of sharded content-generation lookups: what a
//! reader pays to open the index, to load one shard manifest on first touch,
//! and to read an envelope from a loaded shard.
//!
//! It reads generation files directly (no catalog, no recovery), so it never
//! mutates the store. Run against a disposable repacked store:
//! `WHEAT_CONTENT_LATENCY_STORE=/path/to/store cargo test --release -p wheat-store \
//!   --test content_generation_read_latency -- --ignored --nocapture`

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wheat_core::PhysicalContentGeneration;
use wheat_store::{
    content_generation_shard_manifest_path, content_generation_shard_pack_path,
    read_content_generation_index,
};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "needs WHEAT_CONTENT_LATENCY_STORE pointing at a disposable repacked store"]
fn sharded_generation_read_latency() {
    let store = PathBuf::from(
        std::env::var("WHEAT_CONTENT_LATENCY_STORE").expect("WHEAT_CONTENT_LATENCY_STORE"),
    );
    let generations = store.join("storage/content-generations");
    let index_path = fs::read_dir(&generations)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "manifest")
        })
        .expect("one current index");
    let stem = index_path.file_stem().unwrap().to_str().unwrap().to_owned();
    let (generation, digest) = stem.split_once('-').unwrap();
    let authority = PhysicalContentGeneration {
        generation: generation.parse().unwrap(),
        manifest: hex::decode(digest).unwrap().try_into().unwrap(),
    };

    let mut index_times = Vec::new();
    let mut index = None;
    for _ in 0..20 {
        let started = Instant::now();
        index = Some(read_content_generation_index(&generations, authority).unwrap());
        index_times.push(started.elapsed());
    }
    let index = index.unwrap();

    let mut shard_times = Vec::new();
    let mut read_times = Vec::new();
    for shard in 0..index.shards.len() {
        let started = Instant::now();
        let encoded = fs::read(content_generation_shard_manifest_path(
            &generations,
            authority.generation,
            shard,
        ))
        .unwrap();
        let entries = index.decode_shard(shard, &encoded).unwrap();
        shard_times.push(started.elapsed());

        let mut pack = File::open(content_generation_shard_pack_path(
            &generations,
            authority.generation,
            shard,
        ))
        .unwrap();
        // Deterministic spread of entries across the shard.
        for entry in entries.iter().step_by(entries.len().div_ceil(200)) {
            let started = Instant::now();
            let mut envelope = vec![0; entry.physical_length as usize];
            pack.seek(SeekFrom::Start(entry.pack_offset)).unwrap();
            pack.read_exact(&mut envelope).unwrap();
            assert_eq!(blake3::hash(&envelope).as_bytes(), &entry.envelope_digest);
            read_times.push(started.elapsed());
        }
    }
    eprintln!(
        "content generation {}: {} entries in {} shards\n  \
         index load median {:?}\n  \
         shard manifest load+verify median {:?} (max {:?})\n  \
         envelope read+verify median {:?} over {} reads",
        authority.generation,
        index.entry_count,
        index.shards.len(),
        median(index_times),
        median(shard_times.clone()),
        shard_times.iter().max().unwrap(),
        median(read_times.clone()),
        read_times.len(),
    );
}
