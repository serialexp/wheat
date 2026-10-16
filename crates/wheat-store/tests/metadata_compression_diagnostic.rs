//! Bounded, opt-in, read-only physical compression experiment for packed metadata.
//!
//! Run with:
//! `WHEAT_METADATA_COMPRESSION_STORE=/path/to/store cargo test -p wheat-store \
//!   --test metadata_compression_diagnostic -- --ignored --nocapture`

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wheat_store::packed_segment_reader::{SegmentEntry, open_active_segment, open_segment};

const DEFAULT_STORE: &str = "/tmp/wheat-gothab-binschema-Do1n2WGb/store";
const MAX_SEGMENTS: usize = 64;
const MAX_RECORDS: usize = 1_000_000;
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_LOGICAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_SEGMENT_PAYLOAD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SEGMENT_ENTRIES: usize = 1_000_000;
const INDEX_PAGE_ENTRIES: usize = 128;
// Segment framing sizes come from the format so the model cannot drift.
const SEGMENT_PREFIX_BYTES: u64 = wheat_core::SEGMENT_PREFIX_ENCODED_SIZE as u64;
const SEGMENT_FOOTER_BYTES: u64 = wheat_core::SEGMENT_FOOTER_ENCODED_SIZE as u64;
const INDEX_ENTRY_BYTES: u64 = wheat_core::SEGMENT_INDEX_ENTRY_ENCODED_SIZE as u64;
const INDEX_PAGE_COUNT_BYTES: u64 = 4;
const DIRECTORY_ENTRY_BYTES: u64 = 81;
const DIRECTORY_COUNT_BYTES: u64 = 4;
const ZSTD_LEVEL: i32 = 3;
const DELTA_BASE_REFERENCE_BYTES: u64 = (wheat_core::SEGMENT_DELTA_INDEX_ENTRY_ENCODED_SIZE
    - wheat_core::SEGMENT_INDEX_ENTRY_ENCODED_SIZE) as u64;

// Trees and radix/map nodes, the content maps and content sets among them.
const EXPERIMENT_KINDS: &[u16] = &[1, 7, 9, 10, 11, 12, 13, 17, 18];

struct Record {
    kind: u16,
    digest: [u8; 32],
    size_class: u8,
    similarity: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
struct ResultRow {
    payload: u64,
    physical: u64,
    elapsed: Duration,
}

#[derive(Default)]
struct PhysicalAccumulator {
    payload: u64,
    completed: u64,
    current_payload: u64,
    current_entries: usize,
}

impl PhysicalAccumulator {
    fn push(&mut self, stored: usize, entries: usize) -> Result<(), String> {
        let stored = u64::try_from(stored).map_err(|_| "stored length does not fit u64")?;
        if stored > MAX_SEGMENT_PAYLOAD_BYTES || entries > MAX_SEGMENT_ENTRIES {
            return Err("one physical unit exceeds a segment bound".into());
        }
        if self.current_entries != 0
            && (self.current_payload + stored > MAX_SEGMENT_PAYLOAD_BYTES
                || self.current_entries + entries > MAX_SEGMENT_ENTRIES)
        {
            self.seal_current();
        }
        self.payload = self.payload.checked_add(stored).ok_or("payload overflow")?;
        self.current_payload += stored;
        self.current_entries += entries;
        Ok(())
    }

    fn seal_current(&mut self) {
        if self.current_entries == 0 {
            return;
        }
        self.completed +=
            SEGMENT_PREFIX_BYTES + self.current_payload + simulated_tail(self.current_entries);
        self.current_payload = 0;
        self.current_entries = 0;
    }

    fn finish(mut self) -> (u64, u64) {
        self.seal_current();
        (self.payload, self.completed)
    }
}

#[derive(Default)]
struct CurrentPhysical {
    selected_entries: u64,
    selected_logical: u64,
    // Full shared blocks are charged when any selected record uses them. This is
    // deliberately conservative when a block also contains an excluded kind.
    selected_payload_regions: BTreeSet<(PathBuf, u64, u64)>,
}

fn kind_name(kind: u16) -> &'static str {
    match kind {
        1 => "Tree",
        7 => "ContentMapNode",
        9 => "RevisionMapNode",
        10 => "ApplicationMapNode",
        11 => "ConflictSetNode",
        12 => "ContentSetNode",
        13 => "RetainedSetNode",
        17 => "ChangeMapNode",
        18 => "ViewLineageMapNode",
        _ => "unexpected",
    }
}

fn size_class(length: usize) -> u8 {
    if length == 0 {
        0
    } else {
        (usize::BITS - length.leading_zeros()) as u8
    }
}

/// Allocation-free minhash over fixed-size byte shingles. Equal shingles in
/// structurally similar canonical records tend to place them next to each other.
fn similarity_key(bytes: &[u8]) -> u64 {
    if bytes.len() < 16 {
        let hash = blake3::hash(bytes);
        return u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap());
    }
    let mut minimum = u64::MAX;
    for shingle in bytes.chunks(16) {
        let hash = blake3::hash(shingle);
        minimum = minimum.min(u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap()));
    }
    minimum
}

fn simulated_tail(entry_count: usize) -> u64 {
    let entries = entry_count as u64;
    let pages = entry_count.div_ceil(INDEX_PAGE_ENTRIES) as u64;
    entries * INDEX_ENTRY_BYTES
        + pages * INDEX_PAGE_COUNT_BYTES
        + pages * DIRECTORY_ENTRY_BYTES
        + DIRECTORY_COUNT_BYTES
        + SEGMENT_FOOTER_BYTES
}

fn physical_size(entry_count: usize, payload: u64) -> u64 {
    let segments_for_entries = entry_count.div_ceil(MAX_SEGMENT_ENTRIES).max(1);
    let segments_for_payload = payload.div_ceil(MAX_SEGMENT_PAYLOAD_BYTES).max(1) as usize;
    let segment_count = segments_for_entries.max(segments_for_payload);
    // At this corpus scale only payload forces a split. Distributing entries
    // cannot change total entry bytes; page rounding costs at most one partial
    // page per segment and is included by summing each deterministic shard.
    let mut total = payload;
    let mut remaining = entry_count;
    for shard in 0..segment_count {
        let shards_left = segment_count - shard;
        let entries = remaining.div_ceil(shards_left);
        total += SEGMENT_PREFIX_BYTES + simulated_tail(entries);
        remaining -= entries;
    }
    total
}

fn standalone(records: &[Record]) -> Result<ResultRow, String> {
    let started = Instant::now();
    let mut physical = PhysicalAccumulator::default();
    for record in records {
        let compressed = zstd::bulk::compress(&record.bytes, ZSTD_LEVEL)
            .map_err(|error| format!("standalone zstd failed: {error}"))?;
        physical.push(compressed.len(), 1)?;
    }
    let (payload, physical) = physical.finish();
    Ok(ResultRow {
        payload,
        physical,
        elapsed: started.elapsed(),
    })
}

fn block_compress(records: &mut [Record], target: usize) -> Result<ResultRow, String> {
    let started = Instant::now();
    records.sort_unstable_by(|left, right| {
        (left.kind, left.size_class, left.similarity, left.digest).cmp(&(
            right.kind,
            right.size_class,
            right.similarity,
            right.digest,
        ))
    });

    let mut physical = PhysicalAccumulator::default();
    let mut scratch = Vec::with_capacity(target);
    let mut scratch_entries = 0_usize;
    let mut group: Option<(u16, u8)> = None;
    let flush = |scratch: &mut Vec<u8>,
                 scratch_entries: &mut usize,
                 physical: &mut PhysicalAccumulator|
     -> Result<(), String> {
        if scratch.is_empty() {
            return Ok(());
        }
        let compressed = zstd::bulk::compress(scratch, ZSTD_LEVEL)
            .map_err(|error| format!("block zstd failed: {error}"))?;
        physical.push(compressed.len(), *scratch_entries)?;
        scratch.clear();
        *scratch_entries = 0;
        Ok(())
    };

    for record in records.iter() {
        if record.bytes.len() > target {
            flush(&mut scratch, &mut scratch_entries, &mut physical)?;
            group = None;
            let compressed = zstd::bulk::compress(&record.bytes, ZSTD_LEVEL)
                .map_err(|error| format!("oversize standalone zstd failed: {error}"))?;
            physical.push(compressed.len(), 1)?;
            continue;
        }
        let next_group = (record.kind, record.size_class);
        if group.is_some_and(|current| current != next_group)
            || scratch.len() + record.bytes.len() > target
        {
            flush(&mut scratch, &mut scratch_entries, &mut physical)?;
        }
        group = Some(next_group);
        scratch.extend_from_slice(&record.bytes);
        scratch_entries += 1;
    }
    flush(&mut scratch, &mut scratch_entries, &mut physical)?;
    let (payload, physical) = physical.finish();

    Ok(ResultRow {
        payload,
        physical,
        elapsed: started.elapsed(),
    })
}

/// Tests one deterministic predecessor per record after similarity ordering.
/// This is a deliberately shallow, linear candidate probe, not a delta format
/// proposal. Its byte count includes the base digest and zstd-compresses each
/// actual patch, selecting the smaller actual representation.
fn shallow_delta(records: &[Record]) -> Result<ResultRow, String> {
    let started = Instant::now();
    let mut physical = PhysicalAccumulator::default();
    let mut candidates = 0_u64;
    let mut previous: Option<&Record> = None;
    let mut patch = Vec::new();
    for record in records {
        let standalone = zstd::bulk::compress(&record.bytes, ZSTD_LEVEL)
            .map_err(|error| format!("delta standalone zstd failed: {error}"))?;
        let mut chosen = standalone.len() as u64;
        if let Some(base) =
            previous.filter(|base| base.kind == record.kind && base.size_class == record.size_class)
        {
            patch.clear();
            let options = gunnar_delta::Encode {
                max_size: Some(record.bytes.len()),
                ..gunnar_delta::Encode::default()
            };
            if gunnar_delta::delta_into(&base.bytes, &record.bytes, &mut patch, &options)
                .map_err(|error| format!("delta encoding failed: {error}"))?
            {
                candidates += 1;
                let compressed_patch = zstd::bulk::compress(&patch, ZSTD_LEVEL)
                    .map_err(|error| format!("delta patch zstd failed: {error}"))?;
                chosen = chosen.min(
                    DELTA_BASE_REFERENCE_BYTES + (patch.len().min(compressed_patch.len()) as u64),
                );
            }
        }
        physical.push(
            usize::try_from(chosen).map_err(|_| "delta length does not fit usize")?,
            1,
        )?;
        previous = Some(record);
    }
    let (payload, physical) = physical.finish();
    eprintln!("shallow delta probe: {candidates} predecessor candidates encoded");
    Ok(ResultRow {
        payload,
        physical,
        elapsed: started.elapsed(),
    })
}

fn rss_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.trim();
        rest.strip_suffix(" kB")?.trim().parse().ok()
    })
}

fn segment_paths(root: &Path) -> Vec<PathBuf> {
    let directory = root.join("storage/packed/segments");
    let mut paths = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| entry.expect("cannot read segment directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "segment")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no sealed segments in {}",
        directory.display()
    );
    assert!(
        paths.len() <= MAX_SEGMENTS,
        "segment bound {MAX_SEGMENTS} exceeded by {}; refusing to sample",
        paths.len()
    );
    paths
}

fn entry_order(left: &SegmentEntry, right: &SegmentEntry) -> Ordering {
    // Keep shared-block entries adjacent so cache residency is bounded to one
    // physical block without repeatedly decompressing an interleaved block.
    (left.offset, left.stored_length, left.namespace, left.digest).cmp(&(
        right.offset,
        right.stored_length,
        right.namespace,
        right.digest,
    ))
}

fn print_row(name: &str, row: ResultRow, standalone_physical: u64, current_physical: u64) {
    let standalone_saving = standalone_physical as i128 - row.physical as i128;
    let current_saving = current_physical as i128 - row.physical as i128;
    eprintln!(
        "{name:<24} payload={:>10} physical={:>10} vs-current={current_saving:>10} vs-standalone={standalone_saving:>10} bytes time={:?}",
        row.payload, row.physical, row.elapsed
    );
}

#[test]
#[ignore = "opt-in diagnostic over an authenticated local store"]
fn measure_actual_metadata_compression() {
    let total_started = Instant::now();
    let root = std::env::var_os("WHEAT_METADATA_COMPRESSION_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORE));
    assert!(
        root.is_dir(),
        "diagnostic store does not exist: {}",
        root.display()
    );
    let paths = segment_paths(&root);
    let rss_start = rss_kib("VmRSS:").unwrap_or(0);
    let mut records = Vec::new();
    let mut total_logical = 0_usize;
    let mut seen = BTreeSet::new();
    let mut by_kind = BTreeMap::<u16, (u64, u64)>::new();
    let mut current = CurrentPhysical::default();

    for path in &paths {
        let file = File::open(path)
            .unwrap_or_else(|error| panic!("cannot open {}: {error}", path.display()));
        let mut segment = open_segment(file).unwrap_or_else(|error| {
            panic!("authentication failed for {}: {error}", path.display())
        });
        let mut entries = segment.entries();
        entries.sort_unstable_by(entry_order);
        let mut last_block = None;
        for entry in entries {
            if entry.namespace != 1 || !EXPERIMENT_KINDS.contains(&entry.kind) {
                continue;
            }
            assert!(
                records.len() < MAX_RECORDS,
                "record bound {MAX_RECORDS} exceeded; refusing to sample"
            );
            let logical = usize::try_from(entry.logical_length)
                .expect("logical length does not fit this platform");
            assert!(
                logical <= MAX_RECORD_BYTES,
                "record {} exceeds {MAX_RECORD_BYTES}-byte bound",
                hex::encode(entry.digest)
            );
            total_logical = total_logical
                .checked_add(logical)
                .expect("logical byte overflow");
            assert!(
                total_logical <= MAX_TOTAL_LOGICAL_BYTES,
                "logical byte bound {MAX_TOTAL_LOGICAL_BYTES} exceeded; refusing to sample"
            );
            assert!(
                seen.insert((entry.namespace, entry.digest)),
                "duplicate authenticated object {} across scanned segments",
                hex::encode(entry.digest)
            );
            let block = (entry.offset, entry.stored_length, entry.codec);
            if last_block.is_some_and(|previous| previous != block) {
                segment.clear_block_cache();
            }
            last_block = Some(block);
            let bytes = segment
                .read_entry_bounded(entry.namespace, entry.digest, MAX_RECORD_BYTES)
                .unwrap_or_else(|error| {
                    panic!(
                        "payload read failed for {}: {error}",
                        hex::encode(entry.digest)
                    )
                })
                .unwrap_or_else(|| {
                    panic!(
                        "authenticated entry disappeared: {}",
                        hex::encode(entry.digest)
                    )
                });
            assert_eq!(bytes.len(), logical, "authenticated logical length changed");
            assert_eq!(
                wheat_core::validate_canonical_object(&bytes).unwrap_or_else(|error| {
                    panic!(
                        "canonical validation failed for {}: {error}",
                        hex::encode(entry.digest)
                    )
                }),
                wheat_core::SegmentIndexEntry::canonical_kind(entry.kind)
                    .expect("selected kind code must be assigned"),
                "canonical kind differs from authenticated index"
            );

            current.selected_entries += 1;
            current.selected_logical += entry.logical_length;
            current.selected_payload_regions.insert((
                path.clone(),
                entry.offset,
                entry.stored_length,
            ));
            let tally = by_kind.entry(entry.kind).or_default();
            tally.0 += 1;
            tally.1 += entry.logical_length;
            records.push(Record {
                kind: entry.kind,
                digest: entry.digest,
                size_class: size_class(bytes.len()),
                similarity: similarity_key(&bytes),
                bytes,
            });
        }
        segment.clear_block_cache();
    }

    assert!(!records.is_empty(), "no selected metadata records found");
    let current_payload = current
        .selected_payload_regions
        .iter()
        .map(|(_, _, length)| *length)
        .sum::<u64>();
    // Normalize the selected records into the same one-segment physical model as
    // every candidate. Shared current blocks are charged in full; index, page,
    // directory, prefix, and footer bytes are then included exactly.
    let current_charged_physical = physical_size(records.len(), current_payload);
    eprintln!("metadata compression diagnostic: {}", root.display());
    eprintln!(
        "coverage: segments={} records={} logical={} bytes current-charged-payload={} current-charged-physical={} bytes",
        paths.len(),
        records.len(),
        total_logical,
        current_payload,
        current_charged_physical
    );
    for (kind, (count, logical)) in &by_kind {
        eprintln!(
            "  {:<24} count={count:>8} logical={logical:>10}",
            kind_name(*kind)
        );
    }

    let standalone = standalone(&records).expect("standalone experiment failed");
    let block64 = block_compress(&mut records, 64 * 1024).expect("64 KiB experiment failed");
    let block256 = block_compress(&mut records, 256 * 1024).expect("256 KiB experiment failed");
    // block_compress leaves the records in deterministic similarity order.
    let delta = shallow_delta(&records).expect("shallow delta experiment failed");

    eprintln!(
        "physical model: minimum deterministic segment shards bounded to {MAX_SEGMENT_PAYLOAD_BYTES} payload bytes and {MAX_SEGMENT_ENTRIES} records each; prefix={SEGMENT_PREFIX_BYTES}/segment, index={INDEX_ENTRY_BYTES}/record, pages={INDEX_PAGE_ENTRIES} records with {INDEX_PAGE_COUNT_BYTES}+{DIRECTORY_ENTRY_BYTES} bytes/page, directory-count={DIRECTORY_COUNT_BYTES}/segment, footer={SEGMENT_FOOTER_BYTES}/segment"
    );
    print_row(
        "standalone zstd",
        standalone,
        standalone.physical,
        current_charged_physical,
    );
    print_row(
        "64 KiB grouped blocks",
        block64,
        standalone.physical,
        current_charged_physical,
    );
    print_row(
        "256 KiB grouped blocks",
        block256,
        standalone.physical,
        current_charged_physical,
    );
    print_row(
        "shallow predecessor delta",
        delta,
        standalone.physical,
        current_charged_physical,
    );
    let block_incremental = block64.physical.saturating_sub(block256.physical);
    let block_incremental_pct = block_incremental as f64 * 100.0 / block64.physical as f64;
    let recommendation = if block256.physical >= block64.physical {
        "prefer 64 KiB grouped blocks: 256 KiB provides no measured physical saving"
    } else if block_incremental_pct < 1.0 {
        "prefer 64 KiB grouped blocks: 256 KiB saves under 1% more while quadrupling decode scope"
    } else {
        "256 KiB grouped blocks have material measured savings; benchmark random-read latency before choosing over 64 KiB"
    };
    eprintln!(
        "recommendation: {recommendation}; 256-vs-64 incremental={block_incremental} bytes ({block_incremental_pct:.3}%)"
    );
    if delta.physical < block64.physical {
        eprintln!(
            "delta follow-up: shallow predecessor deltas beat 64 KiB blocks by {} bytes; test bounded top-k candidates and random-read reconstruction before format work",
            block64.physical - delta.physical
        );
    } else {
        eprintln!(
            "delta follow-up: do not pursue shallow metadata deltas from this corpus; they are {} bytes larger than 64 KiB blocks",
            delta.physical - block64.physical
        );
    }
    eprintln!(
        "memory/time: rss-start={} KiB rss-end={} KiB VmHWM={} KiB total={:?}",
        rss_start,
        rss_kib("VmRSS:").unwrap_or(0),
        rss_kib("VmHWM:").unwrap_or(0),
        total_started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// History-based metadata deltas (plan step 0: measure before any format work).
//
// Run with:
// `WHEAT_METADATA_COMPRESSION_STORE=/path/to/store cargo test --release \
//   -p wheat-store --test metadata_compression_diagnostic \
//   measure_history_metadata_deltas -- --ignored --nocapture`
//
// Bases come only from view lineage, the same rule content repack uses: for
// every parent -> child edge whose child has exactly one parent, the parent's
// object at a position is a reverse-delta target and the child's object at the
// same position is its base (newer is whole, older is the delta). Positions:
// same directory path (Tree) and same radix edge path (ContentMapNode /
// RevisionMapNode / ApplicationMapNode). Objects claimed by two different
// bases, objects on a cycle, and objects reachable from tip views stay whole.
// Chains stop at depth 16, and a delta is kept only when smaller than whole.
// The segment cut then lays chains out base-first into bounded segments and
// stores a record whole when its base landed in an earlier segment.
// ---------------------------------------------------------------------------

const HISTORY_MAX_OBJECTS: usize = 2_000_000;
const HISTORY_MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
/// Same cap as content (`MAX_DELTA_CHAIN_DEPTH`). Override with
/// `WHEAT_HISTORY_MAX_DEPTH` (1..=255) to measure the depth trade-off.
const HISTORY_MAX_DELTA_DEPTH: u8 = 16;
/// Matches `DEFAULT_REPACK_SEGMENT_BYTES` in `packed/store.rs`.
const HISTORY_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Optional base digest (1 presence + 32) plus a depth byte, charged to every
/// metadata index entry if entries stay fixed-size.
const HISTORY_INDEX_EXTRA_BYTES: u64 = 34;
const HISTORY_METADATA_NAMESPACE: u8 = 1;
const HISTORY_BLOCK_BYTES: usize = 64 * 1024;

type Digest = [u8; 32];

/// Read-only object access over sealed segments, the active segment, and loose
/// envelopes. Every read is authenticated by the segment reader or by
/// recomputing the loose object's ID.
struct HistorySource {
    segments: Vec<wheat_store::packed_segment_reader::AuthenticatedSegment>,
    locations: std::collections::HashMap<Digest, usize>,
    loose: std::collections::HashMap<Digest, PathBuf>,
    /// Current physical charge per metadata object: standalone stored length,
    /// a logical-weighted share of a shared block, or the loose file size.
    current: std::collections::HashMap<Digest, u64>,
    kinds: std::collections::HashMap<Digest, u16>,
    reads: u64,
}

impl HistorySource {
    fn open(storage: &Path) -> Self {
        use std::collections::HashMap;
        let mut paths = Vec::new();
        let segments = storage.join("packed/segments");
        for entry in std::fs::read_dir(&segments).expect("read packed segments") {
            let path = entry.expect("segment directory entry").path();
            if path
                .extension()
                .is_some_and(|extension| extension == "segment")
            {
                paths.push(path);
            }
        }
        paths.sort();
        // The active segment is read first, as it was when it sorted before the
        // sealed segments' directory.
        let active = storage.join("packed/active.segment");
        if active.is_file() {
            paths.insert(0, active.clone());
        }
        assert!(paths.len() <= MAX_SEGMENTS, "segment bound exceeded");

        let mut readers = Vec::with_capacity(paths.len());
        let mut locations = HashMap::new();
        let mut current = HashMap::new();
        let mut kinds = HashMap::new();
        for path in paths {
            let reader = if path == active {
                open_active_segment(&path)
            } else {
                open_segment(File::open(&path).expect("open segment"))
            }
            .unwrap_or_else(|error| panic!("authenticate {}: {error}", path.display()));
            let index = readers.len();
            // Shared blocks are charged to their members by logical share.
            let mut block_logical = HashMap::<(u64, u64), u64>::new();
            let entries = reader.entries();
            for entry in &entries {
                if entry.namespace == HISTORY_METADATA_NAMESPACE {
                    *block_logical
                        .entry((entry.offset, entry.stored_length))
                        .or_default() += entry.logical_length;
                }
            }
            for entry in entries {
                if entry.namespace != HISTORY_METADATA_NAMESPACE {
                    continue;
                }
                if locations.insert(entry.digest, index).is_some() {
                    continue;
                }
                let block = block_logical[&(entry.offset, entry.stored_length)].max(1);
                let share = u128::from(entry.stored_length) * u128::from(entry.logical_length)
                    / u128::from(block);
                current.insert(entry.digest, share as u64);
                kinds.insert(entry.digest, entry.kind);
            }
            readers.push(reader);
        }

        let mut loose = HashMap::new();
        let loose_root = storage.join("loose-objects");
        if loose_root.is_dir() {
            for bucket in std::fs::read_dir(loose_root).expect("read loose objects") {
                let bucket = bucket.expect("loose bucket");
                if !bucket.file_type().expect("bucket type").is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(bucket.path()).expect("read loose bucket") {
                    let entry = entry.expect("loose entry");
                    if !entry.file_type().expect("loose type").is_file() {
                        continue;
                    }
                    let bytes =
                        wheat_store::packed_segment_reader::read_loose_envelope(&entry.path())
                            .unwrap_or_else(|error| {
                                panic!("decode {}: {error}", entry.path().display())
                            });
                    let id = *wheat_core::ObjectId::of_canonical_bytes(&bytes).as_bytes();
                    let kind = wheat_core::validate_canonical_object(&bytes)
                        .expect("loose object is canonical");
                    let code = history_kind_code(kind);
                    if locations.contains_key(&id) || loose.contains_key(&id) {
                        continue;
                    }
                    current.insert(id, entry.metadata().expect("loose metadata").len());
                    kinds.insert(id, code);
                    loose.insert(id, entry.path());
                }
            }
        }
        assert!(
            current.len() <= HISTORY_MAX_OBJECTS,
            "object bound {HISTORY_MAX_OBJECTS} exceeded"
        );
        Self {
            segments: readers,
            locations,
            loose,
            current,
            kinds,
            reads: 0,
        }
    }

    fn read(&mut self, id: Digest) -> Vec<u8> {
        self.reads += 1;
        if let Some(&index) = self.locations.get(&id) {
            let bytes = self.segments[index]
                .read_entry_bounded(HISTORY_METADATA_NAMESPACE, id, HISTORY_MAX_OBJECT_BYTES)
                .unwrap_or_else(|error| panic!("read {}: {error}", hex::encode(id)))
                .unwrap_or_else(|| panic!("indexed object {} disappeared", hex::encode(id)));
            self.segments[index].clear_block_cache();
            return bytes;
        }
        let path = self
            .loose
            .get(&id)
            .unwrap_or_else(|| panic!("object {} is missing", hex::encode(id)));
        wheat_store::packed_segment_reader::read_loose_envelope(path)
            .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()))
    }
}

fn history_kind_code(kind: wheat_core::CanonicalObjectKind) -> u16 {
    use wheat_core::CanonicalObjectKind as K;
    match kind {
        K::Tree => 1,
        K::ContentMapNode => 7,
        K::RevisionMapNode => 9,
        K::ApplicationMapNode => 10,
        _ => 0,
    }
}

fn history_is_paired_kind(code: u16) -> bool {
    matches!(code, 1 | 7 | 9 | 10)
}

fn decode_repository_head(bytes: &[u8]) -> Digest {
    const MAGIC: &[u8] = b"WHEATHEAD";
    assert!(
        bytes.len() == 50 && bytes.starts_with(MAGIC) && bytes[9] == 2,
        "repository.head is corrupt or unsupported"
    );
    bytes[18..50].try_into().unwrap()
}

/// Members of a retained-set radix trie.
fn retained_members(source: &mut HistorySource, root: Digest) -> Vec<Digest> {
    let mut pending = vec![(root, Vec::new())];
    let mut values = Vec::new();
    while let Some((id, mut path)) = pending.pop() {
        let node = wheat_core::decode_retained_set_node(&source.read(id))
            .expect("retained-set node decodes");
        path.extend_from_slice(&node.prefix);
        if node.present {
            values.push(path.as_slice().try_into().expect("32-byte member"));
        }
        for suffix in node.entries {
            let mut value = path.clone();
            value.extend_from_slice(&suffix);
            values.push(value.as_slice().try_into().expect("32-byte member"));
        }
        for child in node.children {
            let mut child_path = path.clone();
            child_path.push(child.edge);
            pending.push((*child.child.as_bytes(), child_path));
        }
    }
    values.sort_unstable();
    values.dedup();
    values
}

/// (parent view, child view) edges from the view-lineage map.
fn lineage_edges(source: &mut HistorySource, root: Digest) -> Vec<(Digest, Digest)> {
    let mut pending = vec![(root, Vec::new())];
    let mut keyed = Vec::new();
    while let Some((id, mut path)) = pending.pop() {
        let node = wheat_core::decode_view_lineage_map_node(&source.read(id))
            .expect("view-lineage node decodes");
        path.extend_from_slice(&node.prefix);
        if let Some(children) = node.value {
            keyed.push((path.clone(), *children.as_bytes()));
        }
        for (suffix, children) in node.entries {
            let mut key = path.clone();
            key.extend_from_slice(&suffix);
            keyed.push((key, *children.as_bytes()));
        }
        for child in node.children {
            let mut child_path = path.clone();
            child_path.push(child.edge);
            pending.push((*child.child.as_bytes(), child_path));
        }
    }
    let mut edges = Vec::new();
    for (parent, children) in keyed {
        let parent: Digest = parent.as_slice().try_into().expect("32-byte view key");
        for child in retained_members(source, children) {
            edges.push((parent, child));
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

#[derive(Clone, Copy)]
struct HistoryView {
    tree: Digest,
    revisions: Digest,
    applications: Digest,
    content: Digest,
}

fn history_view(source: &mut HistorySource, id: Digest) -> HistoryView {
    let view = wheat_core::decode_view_record(&source.read(id)).expect("view decodes");
    HistoryView {
        tree: *view.tree.as_bytes(),
        revisions: *view.revisions.as_bytes(),
        applications: *view.applications.as_bytes(),
        content: *view.required_content.as_bytes(),
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum HistoryBase {
    Unique(Digest),
    Conflict,
}

#[derive(Default)]
struct HistoryProposals {
    bases: BTreeMap<Digest, HistoryBase>,
}

impl HistoryProposals {
    fn propose(&mut self, target: Digest, base: Digest) {
        let proposed = if target == base {
            HistoryBase::Conflict
        } else {
            HistoryBase::Unique(base)
        };
        self.bases
            .entry(target)
            .and_modify(|choice| {
                if *choice != proposed {
                    *choice = HistoryBase::Conflict;
                }
            })
            .or_insert(proposed);
    }
}

/// Radix map children keyed by edge byte, for the two map kinds we pair.
fn map_children(code: u16, bytes: &[u8]) -> BTreeMap<u8, Digest> {
    match code {
        7 => wheat_core::decode_content_map_node(bytes)
            .expect("content map node decodes")
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
        9 => wheat_core::decode_revision_map_node(bytes)
            .expect("revision map node decodes")
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
        10 => wheat_core::decode_application_map_node(bytes)
            .expect("application map node decodes")
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
        _ => unreachable!("only map kinds have radix children"),
    }
}

fn directory_children(bytes: &[u8]) -> BTreeMap<String, Digest> {
    wheat_core::decode_directory_node(bytes)
        .expect("directory decodes")
        .entries
        .into_iter()
        .filter_map(|(name, entry)| match entry {
            wheat_core::DirectoryEntry::Directory(id) => Some((name, *id.as_bytes())),
            wheat_core::DirectoryEntry::File(_) => None,
        })
        .collect()
}

/// Pair the older (target) and newer (base) object at every shared position
/// whose IDs differ, descending only where they differ.
fn pair_tree(
    source: &mut HistorySource,
    proposals: &mut HistoryProposals,
    target: Digest,
    base: Digest,
) {
    let mut pending = vec![(target, base)];
    while let Some((target, base)) = pending.pop() {
        if target == base {
            continue;
        }
        proposals.propose(target, base);
        let older = directory_children(&source.read(target));
        let newer = directory_children(&source.read(base));
        for (name, older_child) in older {
            if let Some(newer_child) = newer.get(&name) {
                pending.push((older_child, *newer_child));
            }
        }
    }
}

fn pair_map(
    source: &mut HistorySource,
    proposals: &mut HistoryProposals,
    code: u16,
    target: Digest,
    base: Digest,
) {
    let mut pending = vec![(target, base)];
    while let Some((target, base)) = pending.pop() {
        if target == base {
            continue;
        }
        proposals.propose(target, base);
        let older = map_children(code, &source.read(target));
        let newer = map_children(code, &source.read(base));
        for (edge, older_child) in older {
            if let Some(newer_child) = newer.get(&edge) {
                pending.push((older_child, *newer_child));
            }
        }
    }
}

/// Every paired-kind object reachable from a view: all directories and all map
/// nodes, the content map's among them.
fn reachable_paired(source: &mut HistorySource, view: HistoryView, into: &mut BTreeSet<Digest>) {
    let mut trees = vec![view.tree];
    while let Some(tree) = trees.pop() {
        if into.insert(tree) {
            trees.extend(directory_children(&source.read(tree)).into_values());
        }
    }
    for (code, root) in [
        (7, view.content),
        (9, view.revisions),
        (10, view.applications),
    ] {
        let mut nodes = vec![root];
        while let Some(node) = nodes.pop() {
            if into.insert(node) {
                nodes.extend(map_children(code, &source.read(node)).into_values());
            }
        }
    }
}

fn whole_cost(bytes: &[u8]) -> u64 {
    let compressed = zstd::bulk::compress(bytes, ZSTD_LEVEL).expect("zstd whole");
    compressed.len().min(bytes.len()) as u64
}

/// Delta payload cost (patch or zstd'd patch, whichever is smaller) plus the
/// in-payload base reference, or `None` when no delta fits under `limit`.
fn delta_cost(base: &[u8], target: &[u8], limit: u64, patch: &mut Vec<u8>) -> Option<u64> {
    patch.clear();
    let options = gunnar_delta::Encode {
        max_size: Some(target.len()),
        ..gunnar_delta::Encode::default()
    };
    if !gunnar_delta::delta_into(base, target, patch, &options).ok()? {
        return None;
    }
    let compressed = zstd::bulk::compress(patch, ZSTD_LEVEL).expect("zstd patch");
    let cost = patch.len().min(compressed.len()) as u64;
    (cost < limit).then_some(cost)
}

#[derive(Clone, Copy)]
struct HistoryChoice {
    whole: u64,
    delta: Option<(Digest, u64)>,
    depth: u8,
    logical: u64,
}

#[derive(Default, Clone, Copy)]
struct KindTotals {
    count: u64,
    logical: u64,
    current: u64,
    whole: u64,
    history: u64,
    history_cut: u64,
    deltas: u64,
}

#[test]
#[ignore = "opt-in diagnostic over an authenticated local store"]
fn measure_history_metadata_deltas() {
    let started = Instant::now();
    let root = std::env::var_os("WHEAT_METADATA_COMPRESSION_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORE));
    let max_depth = std::env::var("WHEAT_HISTORY_MAX_DEPTH")
        .ok()
        .map(|value| {
            value
                .parse::<u8>()
                .expect("WHEAT_HISTORY_MAX_DEPTH is 1..=255")
        })
        .unwrap_or(HISTORY_MAX_DELTA_DEPTH);
    assert!(max_depth >= 1, "WHEAT_HISTORY_MAX_DEPTH must be at least 1");
    let head_before = std::fs::read(root.join("repository.head")).expect("read repository.head");
    let mut source = HistorySource::open(&root.join("storage"));
    let index_elapsed = started.elapsed();

    // Walk the retained graph from the head.
    let envelope = wheat_core::decode_repository_envelope_record(
        &source.read(decode_repository_head(&head_before)),
    )
    .expect("envelope decodes");
    let state =
        wheat_core::decode_repository_state_record(&source.read(*envelope.state.as_bytes()))
            .expect("state decodes");
    let edges = lineage_edges(&mut source, *state.view_lineage.as_bytes());
    let mut parents = BTreeMap::<Digest, Vec<Digest>>::new();
    let mut has_children = BTreeSet::new();
    for (parent, child) in &edges {
        parents.entry(*child).or_default().push(*parent);
        has_children.insert(*parent);
    }
    let mut view_ids = retained_members(&mut source, *state.retained_views.as_bytes());
    view_ids.extend(edges.iter().flat_map(|(parent, child)| [*parent, *child]));
    view_ids.sort_unstable();
    view_ids.dedup();
    let views = view_ids
        .iter()
        .map(|id| (*id, history_view(&mut source, *id)))
        .collect::<BTreeMap<_, _>>();

    // Propose history bases.
    let mut proposals = HistoryProposals::default();
    let mut single_parent_edges = 0_u64;
    for (child, child_parents) in &parents {
        if child_parents.len() != 1 {
            continue;
        }
        single_parent_edges += 1;
        let (Some(older), Some(newer)) = (views.get(&child_parents[0]), views.get(child)) else {
            continue;
        };
        let (older, newer) = (*older, *newer);
        pair_tree(&mut source, &mut proposals, older.tree, newer.tree);
        pair_map(
            &mut source,
            &mut proposals,
            9,
            older.revisions,
            newer.revisions,
        );
        pair_map(
            &mut source,
            &mut proposals,
            10,
            older.applications,
            newer.applications,
        );
        pair_map(&mut source, &mut proposals, 7, older.content, newer.content);
    }
    let mut tips = BTreeSet::new();
    let tip_views = views
        .iter()
        .filter(|(id, _)| !has_children.contains(*id))
        .map(|(_, view)| *view)
        .collect::<Vec<_>>();
    for view in &tip_views {
        reachable_paired(&mut source, *view, &mut tips);
    }
    let conflicts = proposals
        .bases
        .values()
        .filter(|choice| **choice == HistoryBase::Conflict)
        .count();
    let tip_claims = tips
        .iter()
        .filter(|id| proposals.bases.contains_key(*id))
        .count();
    for tip in &tips {
        proposals.bases.remove(tip);
    }
    // A base must be a paired-kind object present in this store.
    proposals.bases.retain(|target, choice| {
        source.kinds.contains_key(target)
            && match choice {
                HistoryBase::Unique(base) => source.kinds.contains_key(base),
                HistoryBase::Conflict => true,
            }
    });
    let pairing_elapsed = started.elapsed();

    // Build chains exactly like content repack: resolve base-first, cap depth,
    // keep a delta only when smaller than whole, cycles stay whole.
    let inventory = source
        .kinds
        .iter()
        .filter(|(_, code)| history_is_paired_kind(**code))
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut chosen = BTreeMap::<Digest, HistoryChoice>::new();
    let mut patch = Vec::new();
    let mut cycles = 0_u64;
    let mut depth_capped = 0_u64;
    let mut not_smaller = 0_u64;
    let mut path = Vec::new();
    let mut on_path = std::collections::HashSet::new();
    let mut bytes_cache = std::collections::HashMap::<Digest, Vec<u8>>::new();
    for start in &inventory {
        if chosen.contains_key(start) {
            continue;
        }
        path.clear();
        on_path.clear();
        let mut cursor = *start;
        let cycle = loop {
            if chosen.contains_key(&cursor)
                || !matches!(proposals.bases.get(&cursor), Some(HistoryBase::Unique(_)))
            {
                break false;
            }
            if !on_path.insert(cursor) {
                break true;
            }
            path.push(cursor);
            let Some(HistoryBase::Unique(base)) = proposals.bases.get(&cursor) else {
                unreachable!()
            };
            cursor = *base;
        };
        if cycle {
            cycles += 1;
        }
        if !chosen.contains_key(&cursor) {
            path.push(cursor);
        }
        for id in path.iter().rev() {
            if chosen.contains_key(id) {
                continue;
            }
            let base = if cycle {
                None
            } else {
                match proposals.bases.get(id) {
                    Some(HistoryBase::Unique(base)) if chosen.contains_key(base) => Some(*base),
                    _ => None,
                }
            };
            let target = bytes_cache.remove(id).unwrap_or_else(|| source.read(*id));
            let whole = whole_cost(&target);
            let mut choice = HistoryChoice {
                whole,
                delta: None,
                depth: 0,
                logical: target.len() as u64,
            };
            if let Some(base_id) = base {
                let base_depth = chosen[&base_id].depth;
                if base_depth >= max_depth {
                    depth_capped += 1;
                } else {
                    let base_bytes = source.read(base_id);
                    let limit = whole.saturating_sub(DELTA_BASE_REFERENCE_BYTES);
                    match delta_cost(&base_bytes, &target, limit, &mut patch) {
                        Some(cost) => {
                            choice.delta = Some((base_id, DELTA_BASE_REFERENCE_BYTES + cost));
                            choice.depth = base_depth + 1;
                        }
                        None => not_smaller += 1,
                    }
                }
            }
            chosen.insert(*id, choice);
            // The next record on this path uses `id` as its base; keep it.
            bytes_cache.clear();
            bytes_cache.insert(*id, target);
        }
        bytes_cache.clear();
    }
    let chain_elapsed = started.elapsed();

    // Segment cut: lay out components base-first in deterministic order.
    let mut dependents = BTreeMap::<Digest, Vec<Digest>>::new();
    let mut roots = Vec::new();
    for (id, choice) in &chosen {
        match choice.delta {
            Some((base, _)) => dependents.entry(base).or_default().push(*id),
            None => roots.push(*id),
        }
    }
    let mut segment_of = std::collections::HashMap::<Digest, u64>::new();
    let mut cut_cost = std::collections::HashMap::<Digest, (u64, u8)>::new();
    let (mut segment, mut fill, mut cuts) = (0_u64, 0_u64, 0_u64);
    let mut stack = Vec::new();
    for root_id in roots {
        stack.push(root_id);
        while let Some(id) = stack.pop() {
            let choice = chosen[&id];
            // A delta stays a delta only if it fits in the segment holding its
            // base; otherwise the chain is cut and this record is stored whole.
            let (cost, depth) = match choice.delta {
                Some((base, delta))
                    if segment_of[&base] == segment && fill + delta <= HISTORY_SEGMENT_BYTES =>
                {
                    (delta, cut_cost[&base].1 + 1)
                }
                Some(_) => {
                    cuts += 1;
                    (choice.whole, 0)
                }
                None => (choice.whole, 0),
            };
            if fill > 0 && fill + cost > HISTORY_SEGMENT_BYTES {
                segment += 1;
                fill = 0;
            }
            fill += cost;
            segment_of.insert(id, segment);
            cut_cost.insert(id, (cost, depth));
            if let Some(children) = dependents.get(&id) {
                stack.extend(children.iter().rev().copied());
            }
        }
    }

    // Whole records may still share 64 KiB zstd blocks (delta records never do).
    let whole_ids = cut_cost
        .iter()
        .filter(|(_, (_, depth))| *depth == 0)
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut whole_records = Vec::with_capacity(whole_ids.len());
    let mut whole_logical = 0_usize;
    for id in &whole_ids {
        let bytes = source.read(*id);
        whole_logical += bytes.len();
        assert!(
            whole_logical <= MAX_TOTAL_LOGICAL_BYTES,
            "whole-record logical bound exceeded"
        );
        whole_records.push(Record {
            kind: source.kinds[id],
            digest: *id,
            size_class: size_class(bytes.len()),
            similarity: similarity_key(&bytes),
            bytes,
        });
    }
    let whole_blocks = block_compress(&mut whole_records, HISTORY_BLOCK_BYTES)
        .expect("whole-record block experiment failed");
    drop(whole_records);

    // All paired-kind records in 64 KiB blocks (no deltas), for comparison.
    let mut all_records = Vec::with_capacity(inventory.len());
    let mut all_logical = 0_usize;
    for id in &inventory {
        let bytes = source.read(*id);
        all_logical += bytes.len();
        assert!(
            all_logical <= MAX_TOTAL_LOGICAL_BYTES,
            "paired-kind logical bound exceeded"
        );
        all_records.push(Record {
            kind: source.kinds[id],
            digest: *id,
            size_class: size_class(bytes.len()),
            similarity: similarity_key(&bytes),
            bytes,
        });
    }
    let all_blocks = block_compress(&mut all_records, HISTORY_BLOCK_BYTES)
        .expect("all-record block experiment failed");
    drop(all_records);

    // Tallies.
    let mut by_kind = BTreeMap::<u16, KindTotals>::new();
    let mut depth_histogram = vec![0_u64; usize::from(max_depth) + 1];
    let mut delta_payload_cut = 0_u64;
    for (id, choice) in &chosen {
        let totals = by_kind.entry(source.kinds[id]).or_default();
        totals.count += 1;
        totals.logical += choice.logical;
        totals.current += source.current[id];
        totals.whole += choice.whole;
        totals.history += choice.delta.map_or(choice.whole, |(_, cost)| cost);
        let (cost, depth) = cut_cost[id];
        totals.history_cut += cost;
        if depth > 0 {
            totals.deltas += 1;
            delta_payload_cut += cost;
        }
        depth_histogram[depth as usize] += 1;
    }
    // Worst-case reconstruction: bytes read along a chain after the cut.
    let mut reconstruct = cut_cost
        .keys()
        .map(|id| {
            let mut total = 0_u64;
            let mut cursor = *id;
            loop {
                let (cost, depth) = cut_cost[&cursor];
                total += cost;
                if depth == 0 {
                    break;
                }
                cursor = chosen[&cursor].delta.expect("delta has a base").0;
            }
            total
        })
        .collect::<Vec<_>>();
    reconstruct.sort_unstable();
    let percentile = |p: f64| reconstruct[((reconstruct.len() - 1) as f64 * p) as usize];

    let sum = |f: fn(&KindTotals) -> u64| by_kind.values().map(f).sum::<u64>();
    let entries = chosen.len() as u64;
    let history_with_blocks = whole_blocks.payload + delta_payload_cut;
    eprintln!("history metadata delta diagnostic: {}", root.display());
    eprintln!(
        "graph: views={} lineage-edges={} single-parent-edges={single_parent_edges} tip-views={} proposals={} conflicts={conflicts} tip-claims-dropped={tip_claims} cycles={cycles}",
        views.len(),
        edges.len(),
        tip_views.len(),
        proposals.bases.len(),
    );
    eprintln!(
        "chains: max-depth={max_depth} objects={entries} depth-capped={depth_capped} delta-not-smaller={not_smaller} segment-cuts={cuts} segments={} (target {HISTORY_SEGMENT_BYTES} bytes)",
        segment + 1
    );
    eprintln!(
        "{:<20} {:>8} {:>12} {:>12} {:>12} {:>12} {:>12} {:>8}",
        "kind", "count", "logical", "current", "whole-zstd", "history", "hist+cut", "deltas"
    );
    for (kind, totals) in &by_kind {
        eprintln!(
            "{:<20} {:>8} {:>12} {:>12} {:>12} {:>12} {:>12} {:>8}",
            kind_name(*kind),
            totals.count,
            totals.logical,
            totals.current,
            totals.whole,
            totals.history,
            totals.history_cut,
            totals.deltas
        );
    }
    eprintln!(
        "{:<20} {:>8} {:>12} {:>12} {:>12} {:>12} {:>12} {:>8}",
        "total",
        sum(|t| t.count),
        sum(|t| t.logical),
        sum(|t| t.current),
        sum(|t| t.whole),
        sum(|t| t.history),
        sum(|t| t.history_cut),
        sum(|t| t.deltas)
    );
    eprintln!(
        "payload scenarios (paired kinds only): current={} all-in-64KiB-blocks={} history+cut, whole records standalone={} history+cut, whole records in 64KiB blocks={}",
        sum(|t| t.current),
        all_blocks.payload,
        sum(|t| t.history_cut),
        history_with_blocks
    );
    eprintln!(
        "index overhead if every metadata entry gains base+depth: {} bytes ({} entries x {HISTORY_INDEX_EXTRA_BYTES})",
        entries * HISTORY_INDEX_EXTRA_BYTES,
        entries
    );
    eprintln!(
        "depth histogram after cut (depth: records): {:?}",
        depth_histogram
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .collect::<Vec<_>>()
    );
    eprintln!(
        "reconstruction bytes read after cut: p50={} p99={} max={}",
        percentile(0.5),
        percentile(0.99),
        percentile(1.0)
    );
    let head_after = std::fs::read(root.join("repository.head")).expect("read repository.head");
    assert_eq!(
        head_before, head_after,
        "repository head changed during diagnostic"
    );
    eprintln!(
        "time: index={index_elapsed:?} pairing={:?} chains={:?} total={:?} reads={} rss-end={} KiB VmHWM={} KiB",
        pairing_elapsed - index_elapsed,
        chain_elapsed - pairing_elapsed,
        started.elapsed(),
        source.reads,
        rss_kib("VmRSS:").unwrap_or(0),
        rss_kib("VmHWM:").unwrap_or(0)
    );
}
