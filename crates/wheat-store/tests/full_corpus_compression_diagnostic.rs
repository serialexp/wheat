//! Read-only, bounded compression experiments over one authenticated content generation.
//!
//! Run explicitly, for example:
//! `WHEAT_COMPRESSION_DIAGNOSTIC_GENERATIONS=/tmp/store/storage/content-generations cargo test -p wheat-store --test full_corpus_compression_diagnostic -- --ignored --nocapture`

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

use wheat_core::ContentId;
use wheat_store::{
    ContentGenerationEntry, ContentGenerationManifest, content_generation_shard_pack_path,
    read_content_generation,
};

const ENVELOPE_BYTES: u64 = 22;
const DELTA_BASE_BYTES: u64 = 32;
const DEFAULT_MAX_ENTRIES: usize = 300_000;
const DEFAULT_MAX_LOGICAL_BYTES: u64 = 1 << 30;
const DEFAULT_MAX_SOURCE_BYTES: u64 = 4 << 30;
const DEFAULT_MAX_CANDIDATE_BYTES: usize = 256 << 20;
const DEFAULT_MAX_RECORD_BYTES: usize = 1 << 20;
const DEFAULT_MAX_DECODE_RECORD_BYTES: usize = 64 << 20;
const DEFAULT_TOP_K: usize = 8;
const DEFAULT_MAX_BUCKET: usize = 128;
const DEFAULT_BLOCK_BYTES: usize = 64 << 10;
const TINY_LIMIT: usize = 1024;
const BLOCK_INDEX_BYTES_PER_RECORD: u64 = 8; // u32 decompressed offset + u32 length.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Codec {
    Raw,
    Zstd,
    Delta,
    DeltaZstd,
}

#[derive(Clone, Copy, Debug)]
struct Header {
    codec: Codec,
    stored: usize,
    logical: usize,
}

#[derive(Default)]
struct SourceBudget {
    bytes: u64,
    reads: u64,
    max: u64,
}

#[derive(Clone)]
struct Candidate {
    id: ContentId,
    bytes: Vec<u8>,
    features: [u64; 4],
    current_physical: u64,
}

#[derive(Default)]
struct Totals {
    bytes: u64,
    records: u64,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default)
}

fn parse_header(bytes: &[u8]) -> Header {
    assert_eq!(bytes.len(), ENVELOPE_BYTES as usize);
    assert_eq!(&bytes[..4], b"WHCE", "bad content envelope magic");
    assert_eq!(bytes[4], 1, "unsupported content envelope version");
    let codec = match bytes[5] {
        0 => Codec::Raw,
        1 => Codec::Zstd,
        2 => Codec::Delta,
        3 => Codec::DeltaZstd,
        other => panic!("unsupported content codec {other}"),
    };
    Header {
        codec,
        stored: usize::try_from(u64::from_be_bytes(bytes[6..14].try_into().unwrap())).unwrap(),
        logical: usize::try_from(u64::from_be_bytes(bytes[14..22].try_into().unwrap())).unwrap(),
    }
}

fn read_authenticated(
    pack: &mut File,
    entry: &ContentGenerationEntry,
    budget: &mut SourceBudget,
) -> (Header, Vec<u8>) {
    let physical = usize::try_from(entry.physical_length).expect("physical length fits usize");
    assert!(physical >= ENVELOPE_BYTES as usize);
    budget.bytes = budget
        .bytes
        .checked_add(entry.physical_length)
        .expect("source-byte counter overflow");
    budget.reads += 1;
    assert!(
        budget.bytes <= budget.max,
        "source-read budget {} exceeded after {} reads; raise WHEAT_COMPRESSION_DIAGNOSTIC_MAX_SOURCE_BYTES explicitly",
        budget.max,
        budget.reads
    );
    let mut envelope = vec![0; physical];
    pack.seek(SeekFrom::Start(entry.pack_offset)).unwrap();
    pack.read_exact(&mut envelope).unwrap();
    assert_eq!(
        blake3::hash(&envelope).as_bytes(),
        &entry.envelope_digest,
        "envelope digest mismatch for {}",
        entry.id
    );
    let header = parse_header(&envelope[..ENVELOPE_BYTES as usize]);
    assert_eq!(header.stored + ENVELOPE_BYTES as usize, physical);
    assert_eq!(header.logical as u64, entry.logical_length);
    (header, envelope.split_off(ENVELOPE_BYTES as usize))
}

fn decode_delta(
    base: &[u8],
    patch: &[u8],
    expected_output_len: usize,
    max_record: usize,
) -> Vec<u8> {
    fn varint(input: &[u8], cursor: &mut usize) -> usize {
        let mut value = 0usize;
        let mut shift = 0;
        loop {
            let byte = *input.get(*cursor).expect("truncated delta varint");
            *cursor += 1;
            value |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
            assert!(shift < usize::BITS as usize, "delta varint overflow");
        }
    }

    let mut cursor = 0;
    assert_eq!(
        varint(patch, &mut cursor),
        base.len(),
        "delta base length differs"
    );
    let output_len = varint(patch, &mut cursor);
    assert_eq!(
        output_len, expected_output_len,
        "delta header output length differs from authenticated manifest"
    );
    assert!(
        output_len <= max_record,
        "delta output exceeds configured decode-record bound before allocation"
    );
    let mut output = Vec::with_capacity(output_len);
    while cursor < patch.len() {
        let command = patch[cursor];
        cursor += 1;
        if command & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for bit in 0..4 {
                if command & (1 << bit) != 0 {
                    offset |= usize::from(patch[cursor]) << (8 * bit);
                    cursor += 1;
                }
            }
            for bit in 0..3 {
                if command & (1 << (4 + bit)) != 0 {
                    size |= usize::from(patch[cursor]) << (8 * bit);
                    cursor += 1;
                }
            }
            if size == 0 {
                size = 0x1_0000;
            }
            let end = offset
                .checked_add(size)
                .expect("delta copy offset overflow");
            let next_len = output
                .len()
                .checked_add(size)
                .expect("delta output length overflow");
            assert!(
                next_len <= expected_output_len,
                "delta copy exceeds authenticated output length"
            );
            output.extend_from_slice(base.get(offset..end).expect("delta copy out of range"));
        } else {
            let size = usize::from(command);
            assert_ne!(size, 0, "reserved delta command");
            let patch_end = cursor
                .checked_add(size)
                .expect("delta insert offset overflow");
            let next_len = output
                .len()
                .checked_add(size)
                .expect("delta output length overflow");
            assert!(
                next_len <= expected_output_len,
                "delta insert exceeds authenticated output length"
            );
            output.extend_from_slice(
                patch
                    .get(cursor..patch_end)
                    .expect("delta insert truncated"),
            );
            cursor = patch_end;
        }
    }
    assert_eq!(output.len(), output_len, "delta output length differs");
    output
}

fn decode_entry(
    index: usize,
    entries: &[ContentGenerationEntry],
    by_id: &BTreeMap<ContentId, usize>,
    packs: &mut [File],
    budget: &mut SourceBudget,
    max_depth: usize,
    max_decode_record: usize,
) -> Vec<u8> {
    let mut chain: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut cursor = index;
    for depth in 0..=max_depth {
        let entry = &entries[cursor];
        assert!(
            entry.logical_length <= max_decode_record as u64,
            "manifest record exceeds configured decode-record bound"
        );
        let pack = &mut packs[ContentGenerationManifest::shard_of(cursor)];
        let (header, stored) = read_authenticated(pack, entry, budget);
        match header.codec {
            Codec::Raw => {
                assert_eq!(stored.len(), header.logical);
                assert!(
                    stored.len() <= max_decode_record,
                    "raw record exceeds configured decode-record bound"
                );
                let mut result = stored;
                for (expected_len, patch) in chain.into_iter().rev() {
                    result = decode_delta(&result, &patch, expected_len, max_decode_record);
                    assert_eq!(result.len(), expected_len);
                }
                assert_eq!(
                    ContentId::of(&result),
                    entries[index].id,
                    "logical digest differs"
                );
                return result;
            }
            Codec::Zstd => {
                assert!(
                    header.logical <= max_decode_record,
                    "zstd record exceeds configured decode-record bound before allocation"
                );
                let mut result = zstd::bulk::decompress(&stored, header.logical).unwrap();
                assert_eq!(result.len(), header.logical);
                for (expected_len, patch) in chain.into_iter().rev() {
                    result = decode_delta(&result, &patch, expected_len, max_decode_record);
                    assert_eq!(result.len(), expected_len);
                }
                assert_eq!(
                    ContentId::of(&result),
                    entries[index].id,
                    "logical digest differs"
                );
                return result;
            }
            Codec::Delta | Codec::DeltaZstd => {
                assert!(stored.len() >= DELTA_BASE_BYTES as usize);
                let base = ContentId::from_digest(stored[..32].try_into().unwrap());
                assert_eq!(Some(base), entry.delta_base, "manifest delta base differs");
                let patch = if header.codec == Codec::DeltaZstd {
                    zstd::bulk::decompress(&stored[32..], max_decode_record).unwrap()
                } else {
                    stored[32..].to_vec()
                };
                chain.push((header.logical, patch));
                cursor = *by_id
                    .get(&base)
                    .expect("delta base absent from current manifest");
            }
        }
        assert!(depth < max_depth, "delta depth exceeds configured bound");
    }
    unreachable!()
}

fn features(bytes: &[u8]) -> [u64; 4] {
    if bytes.is_empty() {
        return [0; 4];
    }
    let width = bytes.len().min(32);
    let last = bytes.len() - width;
    let offsets = [0, last / 3, last.saturating_mul(2) / 3, last];
    offsets.map(|offset| {
        let digest = blake3::hash(&bytes[offset..offset + width]);
        u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap())
    })
}

fn current_rss_bytes() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn pct(saved: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        saved as f64 * 100.0 / baseline as f64
    }
}

#[test]
#[ignore = "read-only full-corpus diagnostic; set WHEAT_COMPRESSION_DIAGNOSTIC_GENERATIONS"]
fn report_full_corpus_compression_opportunities() {
    let started = Instant::now();
    let generations = PathBuf::from(
        std::env::var("WHEAT_COMPRESSION_DIAGNOSTIC_GENERATIONS")
            .expect("set WHEAT_COMPRESSION_DIAGNOSTIC_GENERATIONS to storage/content-generations"),
    );
    let max_entries = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_ENTRIES",
        DEFAULT_MAX_ENTRIES,
    );
    let max_logical = env_u64(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_LOGICAL_BYTES",
        DEFAULT_MAX_LOGICAL_BYTES,
    );
    let max_record = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_RECORD_BYTES",
        DEFAULT_MAX_RECORD_BYTES,
    );
    let max_decode_record = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_DECODE_RECORD_BYTES",
        DEFAULT_MAX_DECODE_RECORD_BYTES,
    );
    let max_candidate_bytes = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_CANDIDATE_BYTES",
        DEFAULT_MAX_CANDIDATE_BYTES,
    );
    let top_k = env_usize("WHEAT_COMPRESSION_DIAGNOSTIC_TOP_K", DEFAULT_TOP_K);
    let max_bucket = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_BUCKET",
        DEFAULT_MAX_BUCKET,
    );
    let block_bytes = env_usize(
        "WHEAT_COMPRESSION_DIAGNOSTIC_BLOCK_BYTES",
        DEFAULT_BLOCK_BYTES,
    );
    assert!(top_k > 0 && max_bucket > 0 && block_bytes >= TINY_LIMIT);

    let mut manifests = fs::read_dir(&generations)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "manifest")
        })
        .collect::<Vec<_>>();
    manifests.sort();
    assert_eq!(
        manifests.len(),
        1,
        "diagnostic requires an explicit directory containing exactly one current manifest"
    );
    let filename = manifests[0].file_stem().unwrap().to_string_lossy();
    let (generation, digest_hex) = filename
        .split_once('-')
        .expect("manifest filename is generation-digest");
    let manifest = read_content_generation(
        &generations,
        wheat_core::PhysicalContentGeneration {
            generation: generation.parse().expect("manifest generation"),
            manifest: hex::decode(digest_hex)
                .expect("manifest digest hex")
                .try_into()
                .expect("manifest digest length"),
        },
    )
    .expect("manifest authentication failed");
    assert!(
        manifest.entries.len() <= max_entries,
        "entry bound exceeded"
    );
    let logical_total = manifest
        .entries
        .iter()
        .try_fold(0u64, |sum, entry| sum.checked_add(entry.logical_length))
        .unwrap();
    assert!(logical_total <= max_logical, "logical-byte bound exceeded");
    let physical_total = manifest
        .entries
        .iter()
        .map(|entry| entry.physical_length)
        .sum::<u64>();
    let full_current = manifest
        .entries
        .iter()
        .filter(|entry| entry.delta_base.is_none())
        .map(|entry| entry.physical_length)
        .sum::<u64>();
    let delta_current = physical_total - full_current;
    let full_count = manifest
        .entries
        .iter()
        .filter(|entry| entry.delta_base.is_none())
        .count();
    let delta_count = manifest.entries.len() - full_count;
    let max_depth = manifest
        .entries
        .iter()
        .map(|entry| entry.delta_depth)
        .max()
        .unwrap_or(0);
    eprintln!(
        "manifest={} generation={} records={} logical={} physical={} full={}/{} delta={}/{} max_depth={} rss={} elapsed={:?}",
        manifests[0].display(),
        manifest.generation,
        manifest.entries.len(),
        logical_total,
        physical_total,
        full_count,
        full_current,
        delta_count,
        delta_current,
        max_depth,
        current_rss_bytes(),
        started.elapsed()
    );

    let by_id = manifest
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id, index))
        .collect::<BTreeMap<_, _>>();
    let mut packs = (0..manifest.shard_count())
        .map(|shard| {
            File::open(content_generation_shard_pack_path(
                &generations,
                manifest.generation,
                shard,
            ))
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        packs
            .iter()
            .map(|pack| pack.metadata().unwrap().len())
            .sum::<u64>(),
        physical_total,
        "pack lengths differ from manifest"
    );
    let mut budget = SourceBudget {
        max: env_u64(
            "WHEAT_COMPRESSION_DIAGNOSTIC_MAX_SOURCE_BYTES",
            DEFAULT_MAX_SOURCE_BYTES,
        ),
        ..Default::default()
    };
    let levels = std::env::var("WHEAT_COMPRESSION_DIAGNOSTIC_ZSTD_LEVELS")
        .unwrap_or_else(|_| "1,3,6,9".into())
        .split(',')
        .map(|value| {
            value
                .parse::<i32>()
                .expect("zstd levels must be comma-separated integers")
        })
        .collect::<Vec<_>>();
    let thresholds = [0usize, 256, 512, 1024];
    let mut all_full_zstd = levels
        .iter()
        .map(|level| (*level, Totals::default()))
        .collect::<BTreeMap<_, _>>();
    let mut current_full_zstd = levels
        .iter()
        .flat_map(|level| thresholds.map(|threshold| ((*level, threshold), Totals::default())))
        .collect::<BTreeMap<_, _>>();
    let mut zstd_wins = levels
        .iter()
        .map(|level| (*level, 0u64))
        .collect::<BTreeMap<_, _>>();
    let mut tiny = Vec::new();
    let mut candidates = Vec::new();
    let mut retained_bytes = 0usize;

    for (index, entry) in manifest.entries.iter().enumerate() {
        let logical = decode_entry(
            index,
            &manifest.entries,
            &by_id,
            &mut packs,
            &mut budget,
            usize::from(max_depth),
            max_decode_record,
        );
        for level in &levels {
            let compressed = zstd::bulk::compress(&logical, *level).unwrap();
            let candidate = ENVELOPE_BYTES + compressed.len() as u64;
            let raw = ENVELOPE_BYTES + logical.len() as u64;
            let chosen = candidate.min(raw);
            let total = all_full_zstd.get_mut(level).unwrap();
            total.bytes += chosen;
            total.records += 1;
            if candidate < raw {
                *zstd_wins.get_mut(level).unwrap() += 1;
            }
            if entry.delta_base.is_none() {
                for threshold in thresholds {
                    let total = current_full_zstd.get_mut(&(*level, threshold)).unwrap();
                    total.bytes += if logical.len() >= threshold {
                        chosen
                    } else {
                        raw
                    };
                    total.records += 1;
                }
            }
        }
        if entry.delta_base.is_none() && logical.len() < TINY_LIMIT {
            tiny.push((entry.id, logical.clone(), entry.physical_length));
        }
        if logical.len() <= max_record
            && retained_bytes.saturating_add(logical.len()) <= max_candidate_bytes
        {
            retained_bytes += logical.len();
            candidates.push(Candidate {
                id: entry.id,
                features: features(&logical),
                bytes: logical,
                current_physical: entry.physical_length,
            });
        }
        if index % 25_000 == 0 {
            eprintln!(
                "decoded={}/{} source_reads={} source_bytes={} retained_records={} retained_bytes={} rss={} elapsed={:?}",
                index + 1,
                manifest.entries.len(),
                budget.reads,
                budget.bytes,
                candidates.len(),
                retained_bytes,
                current_rss_bytes(),
                started.elapsed()
            );
        }
    }
    for level in &levels {
        let total = &all_full_zstd[level];
        eprintln!(
            "all-logical-full-zstd level={} bytes={} (not comparable to current deltas) compressed_wins={}/{} exact_cost=envelope(22)+min(raw,zstd)",
            level, total.bytes, zstd_wins[level], total.records
        );
        for threshold in thresholds {
            let total = &current_full_zstd[&(*level, threshold)];
            let saved = full_current.saturating_sub(total.bytes);
            eprintln!(
                "current-full-zstd level={} threshold={} bytes={} saved={} ({:.2}%) records={} exact_cost=envelope(22)+selected(raw-or-zstd)",
                level,
                threshold,
                total.bytes,
                saved,
                pct(saved, full_current),
                total.records
            );
        }
    }

    tiny.sort_by_key(|(id, _, _)| *id);
    let tiny_current = tiny.iter().map(|(_, _, physical)| *physical).sum::<u64>();
    for level in &levels {
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        let mut scratch = Vec::with_capacity(block_bytes);
        let mut count = 0u64;
        for (_, record, _) in &tiny {
            if !scratch.is_empty() && scratch.len() + record.len() > block_bytes {
                bytes += ENVELOPE_BYTES
                    + zstd::bulk::compress(&scratch, *level).unwrap().len() as u64
                    + count * BLOCK_INDEX_BYTES_PER_RECORD;
                blocks += 1;
                scratch.clear();
                count = 0;
            }
            scratch.extend_from_slice(record);
            count += 1;
        }
        if count != 0 {
            bytes += ENVELOPE_BYTES
                + zstd::bulk::compress(&scratch, *level).unwrap().len() as u64
                + count * BLOCK_INDEX_BYTES_PER_RECORD;
            blocks += 1;
        }
        let saved = tiny_current.saturating_sub(bytes);
        eprintln!(
            "tiny-shared-block level={} records={} blocks={} raw_enveloped={} candidate={} saved={} ({:.2}%) exact_cost=22/block+zstd_frame+8/record-index",
            level,
            tiny.len(),
            blocks,
            tiny_current,
            bytes,
            saved,
            pct(saved, tiny_current)
        );
    }

    // Four deterministic content features provide bounded candidate generation.
    // Buckets retain at most `max_bucket` prior indices; no all-pairs scan occurs.
    let mut buckets = BTreeMap::<u64, Vec<usize>>::new();
    let mut best_delta_bytes = 0u64;
    let mut candidate_baseline = 0u64;
    let mut improved = 0u64;
    let mut pair_evaluations = 0u64;
    let mut truncated_buckets = 0u64;
    for target_index in 0..candidates.len() {
        let target = &candidates[target_index];
        let baseline = target.current_physical;
        candidate_baseline += baseline;
        let mut scores = BTreeMap::<usize, u8>::new();
        for feature in target.features {
            if let Some(bucket) = buckets.get(&feature) {
                for &base_index in bucket {
                    *scores.entry(base_index).or_default() += 1;
                }
            }
        }
        let mut ranked = BinaryHeap::<Reverse<(u8, ContentId, usize)>>::new();
        for (base_index, score) in scores {
            let key = Reverse((score, candidates[base_index].id, base_index));
            ranked.push(key);
            if ranked.len() > top_k {
                ranked.pop();
            }
        }
        let mut best = baseline;
        for Reverse((_, _, base_index)) in ranked.into_iter() {
            pair_evaluations += 1;
            let mut patch = Vec::new();
            let options = gunnar_delta::Encode {
                max_size: Some(target.bytes.len()),
                ..Default::default()
            };
            if gunnar_delta::delta_into(
                &candidates[base_index].bytes,
                &target.bytes,
                &mut patch,
                &options,
            )
            .unwrap_or(false)
            {
                let raw = ENVELOPE_BYTES + DELTA_BASE_BYTES + patch.len() as u64;
                let compressed = levels
                    .iter()
                    .map(|level| zstd::bulk::compress(&patch, *level).unwrap().len() as u64)
                    .min()
                    .unwrap_or(patch.len() as u64);
                best = best.min(raw.min(ENVELOPE_BYTES + DELTA_BASE_BYTES + compressed));
            }
        }
        if best < baseline {
            improved += 1;
        }
        best_delta_bytes += best;
        for feature in BTreeSet::from(target.features) {
            let bucket = buckets.entry(feature).or_default();
            if bucket.len() == max_bucket {
                bucket.remove(0);
                truncated_buckets += 1;
            }
            bucket.push(target_index);
        }
    }
    let saved = candidate_baseline.saturating_sub(best_delta_bytes);
    eprintln!(
        "similarity-top-k records={} retained_logical={} max_record={} top_k={} max_bucket={} pair_evaluations={} (bound={}) truncated_bucket_insertions={} baseline={} candidate={} improved={} saved={} ({:.2}%) exact_delta_cost=22-envelope+32-base+min(raw_patch,zstd_patch) rss={} elapsed={:?}",
        candidates.len(),
        retained_bytes,
        max_record,
        top_k,
        max_bucket,
        pair_evaluations,
        candidates.len().saturating_mul(top_k),
        truncated_buckets,
        candidate_baseline,
        best_delta_bytes,
        improved,
        saved,
        pct(saved, candidate_baseline),
        current_rss_bytes(),
        started.elapsed()
    );
    eprintln!(
        "coverage current_manifest_records={} decoded={} candidate_records={} tiny_current_full_records={} source_reads={} source_bytes={} source_budget={} elapsed={:?}",
        manifest.entries.len(),
        manifest.entries.len(),
        candidates.len(),
        tiny.len(),
        budget.reads,
        budget.bytes,
        budget.max,
        started.elapsed()
    );
}
