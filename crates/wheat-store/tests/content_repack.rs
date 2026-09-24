use std::sync::Mutex;

use wheat_core::ContentId;
use wheat_store::{
    CONTENT_GENERATION_SHARD_ENTRIES, ContentGenerationEntry, ContentGenerationIndex,
    ContentGenerationManifest, ContentGenerationTarget, ContentRepackInput, ContentRepackLimits,
    ContentRepackPlan, ContentStore, Durability, PreparedContentGeneration, plan_content_repack,
    prepare_content_repack, publish_content_repack,
};

/// Encode a generation to its index and shard manifests, then decode it back.
fn round_trip(manifest: &ContentGenerationManifest) -> ContentGenerationManifest {
    let index = manifest.index().unwrap().encoded().unwrap();
    let shards = (0..manifest.shard_count())
        .map(|shard| manifest.encode_shard(shard).unwrap())
        .collect::<Vec<_>>();
    ContentGenerationManifest::decode(&index, |shard| Ok(shards[shard].clone())).unwrap()
}

/// `index` with shard `shard` re-pointed at `forged`, so decoding reaches the
/// checks behind the digest.
fn forge_shard(
    index: &ContentGenerationIndex,
    shard: usize,
    forged: &[u8],
) -> ContentGenerationIndex {
    let mut index = index.clone();
    index.shards[shard].manifest = *blake3::hash(forged).as_bytes();
    index
}

fn version(n: usize) -> Vec<u8> {
    let mut bytes = (0..8 * 1024)
        .map(|offset| ((offset * 131 + offset / 7) % 251) as u8)
        .collect::<Vec<_>>();
    bytes[100..108].copy_from_slice(&(n as u64).to_be_bytes());
    bytes
}

#[test]
fn reverse_plan_preserves_inventory_and_forces_every_tip_full() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let bytes = (0..5).map(version).collect::<Vec<_>>();
    let ids = bytes
        .iter()
        .map(|bytes| source.put(bytes).unwrap())
        .collect::<Vec<_>>();
    let orphan = source.put(b"authenticated but outside lineages").unwrap();

    let plan = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: ids.iter().copied().chain([orphan]).collect(),
            lineages_newest_first: vec![vec![ids[4], ids[3], ids[2]], vec![ids[2], ids[1], ids[0]]],
        },
        ContentRepackLimits::default(),
    )
    .unwrap();
    let ContentRepackPlan::Replace(prepared) = plan else {
        panic!("first plan cannot be a no-op")
    };

    assert_eq!(prepared.manifest.entries.len(), 6);
    for tip in [ids[4], ids[2]] {
        let entry = prepared
            .manifest
            .entries
            .iter()
            .find(|entry| entry.id == tip)
            .unwrap();
        assert_eq!(entry.delta_base, None);
        assert_eq!(entry.delta_depth, 0);
    }
    prepared.manifest.verify().unwrap();
    assert_eq!(round_trip(&prepared.manifest), prepared.manifest);
    let orphan_entry = prepared
        .manifest
        .entries
        .iter()
        .find(|entry| entry.id == orphan)
        .unwrap();
    assert_eq!(orphan_entry.delta_base, None);
}

#[test]
fn binary_index_and_shard_reject_oversized_counts_and_noncanonical_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let id = source.put(b"binary manifest").unwrap();
    let ContentRepackPlan::Replace(prepared) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: vec![id],
            lineages_newest_first: vec![],
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        panic!("first plan cannot be a no-op");
    };
    let manifest = prepared.manifest;
    let index = manifest.index().unwrap();
    let encoded_index = index.encoded().unwrap();
    let shard = manifest.encode_shard(0).unwrap();
    assert_eq!(&encoded_index[..4], b"WHCM");
    assert_eq!(&encoded_index[4..6], &1_u16.to_be_bytes());
    assert_eq!(&shard[..4], b"WHCS");
    assert_eq!(&shard[4..6], &1_u16.to_be_bytes());
    assert_eq!(
        ContentGenerationIndex::decode(&encoded_index).unwrap(),
        index
    );
    assert_eq!(round_trip(&manifest), manifest);

    let malformed = |bytes: &[u8], edit: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = bytes.to_vec();
        edit(&mut bytes);
        bytes
    };
    // Index header: magic, version, generation, entry count (u64), then the
    // u32 shard count at 22..26.
    for (label, bytes) in [
        ("JSON", br#"{\"generation\":1}"#.to_vec()),
        (
            "wrong magic",
            malformed(&encoded_index, &|b| b[..4].copy_from_slice(b"JSON")),
        ),
        (
            "wrong version",
            malformed(&encoded_index, &|b| {
                b[4..6].copy_from_slice(&2_u16.to_be_bytes())
            }),
        ),
        (
            "forged shard count",
            malformed(&encoded_index, &|b| {
                b[22..26].copy_from_slice(&u32::MAX.to_be_bytes())
            }),
        ),
        (
            "entry count disagrees with shards",
            malformed(&encoded_index, &|b| {
                b[14..22].copy_from_slice(&2_u64.to_be_bytes())
            }),
        ),
        ("trailing byte", malformed(&encoded_index, &|b| b.push(0))),
    ] {
        assert!(
            ContentGenerationIndex::decode(&bytes).is_err(),
            "index {label}"
        );
    }
    // Shard header: magic, version, generation, shard number, then the u32
    // entry count at 18..22.
    for (label, bytes) in [
        (
            "wrong magic",
            malformed(&shard, &|b| b[..4].copy_from_slice(b"WHCM")),
        ),
        (
            "wrong version",
            malformed(&shard, &|b| b[4..6].copy_from_slice(&2_u16.to_be_bytes())),
        ),
        (
            "more entries than a shard holds",
            malformed(&shard, &|b| {
                b[18..22]
                    .copy_from_slice(&(CONTENT_GENERATION_SHARD_ENTRIES as u32 + 1).to_be_bytes())
            }),
        ),
        ("trailing byte", malformed(&shard, &|b| b.push(0))),
    ] {
        assert!(
            wheat_core::decode_content_generation_shard(&bytes).is_err(),
            "shard {label}"
        );
        assert!(
            forge_shard(&index, 0, &bytes)
                .decode_shard(0, &bytes)
                .is_err(),
            "shard {label}"
        );
    }
    // Any change to a shard is caught by the digest the index names.
    assert!(
        index
            .decode_shard(0, &malformed(&shard, &|b| *b.last_mut().unwrap() ^= 1))
            .unwrap_err()
            .to_string()
            .contains("digest")
    );

    // Shard header is 22 bytes; a ContentId starts with its algorithm discriminator.
    let wrong_algorithm = malformed(&shard, &|b| b[22] = 2);
    assert!(
        forge_shard(&index, 0, &wrong_algorithm)
            .decode_shard(0, &wrong_algorithm)
            .unwrap_err()
            .to_string()
            .contains("content-id algorithm")
    );

    // The sole entry's optional-base discriminator follows the fixed 33-byte
    // ContentId, digest, and three u64 fields. Claim a base then cut its ID.
    let truncated_optional_base = malformed(&shard, &|b| {
        b[22 + 33 + 32 + 24] = 1;
        b.truncate(22 + 33 + 32 + 24 + 1);
    });
    assert!(wheat_core::decode_content_generation_shard(&truncated_optional_base).is_err());
}

#[test]
fn shard_decoding_rejects_noncanonical_entry_order() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let first = source.put(b"first binary manifest entry").unwrap();
    let second = source.put(b"second binary manifest entry").unwrap();
    let ContentRepackPlan::Replace(prepared) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: vec![first, second],
            lineages_newest_first: vec![],
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        panic!("first plan cannot be a no-op");
    };
    let index = prepared.manifest.index().unwrap();
    let mut encoded = prepared.manifest.encode_shard(0).unwrap();
    // Entries are fixed-width here because neither has a delta base. Exchange
    // their complete ContentIds, retaining a generated-canonical byte shape.
    const HEADER: usize = 22;
    const ENTRY_SIZE: usize = 91;
    let (before_second, second_and_after) = encoded.split_at_mut(HEADER + ENTRY_SIZE);
    before_second[HEADER..HEADER + 33].swap_with_slice(&mut second_and_after[..33]);
    let error = forge_shard(&index, 0, &encoded)
        .decode_shard(0, &encoded)
        .unwrap_err()
        .to_string();
    // The first entry no longer matches the index, or the order is wrong:
    // both are ordering failures.
    assert!(error.contains("strictly ordered"), "{error}");
}

#[test]
fn depth_greater_than_one_resolves_after_generation_import() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(source_dir.path());
    let bytes = (0..6).map(version).collect::<Vec<_>>();
    let ids = bytes
        .iter()
        .map(|bytes| source.put(bytes).unwrap())
        .collect::<Vec<_>>();
    let ContentRepackPlan::Replace(analysis) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: ids.clone(),
            lineages_newest_first: vec![ids.iter().rev().copied().collect()],
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    assert!(
        analysis
            .manifest
            .entries
            .iter()
            .any(|entry| entry.delta_depth > 1)
    );

    let prepared = prepare_content_repack(&source, &analysis).unwrap();
    let imported_dir = tempfile::tempdir().unwrap();
    let imported = ContentStore::new(imported_dir.path());
    // Import in the scratch stream's dependency-first order. The complete
    // generation remains invisible until publication.
    for envelope in prepared.envelopes().unwrap() {
        let envelope = envelope.unwrap();
        let path = imported.path(envelope.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &envelope.bytes).unwrap();
    }
    for (id, expected) in ids.into_iter().zip(bytes) {
        assert_eq!(imported.get(id).unwrap(), expected);
    }
}

#[test]
fn deterministic_plan_has_noop_fingerprint_and_depth_cap() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let bytes = (0..20).map(version).collect::<Vec<_>>();
    let ids = bytes
        .iter()
        .map(|bytes| source.put(bytes).unwrap())
        .collect::<Vec<_>>();
    let input = || ContentRepackInput {
        content_ids: ids.iter().rev().copied().collect(),
        lineages_newest_first: vec![ids.iter().rev().copied().collect()],
    };
    let ContentRepackPlan::Replace(first) =
        plan_content_repack(&source, None, input(), ContentRepackLimits::default()).unwrap()
    else {
        unreachable!()
    };
    assert!(
        first
            .manifest
            .entries
            .iter()
            .all(|entry| entry.delta_depth <= 16)
    );
    let decoded = round_trip(&first.manifest);
    assert_eq!(
        decoded.content_fingerprint,
        first.manifest.content_fingerprint
    );
    assert_eq!(decoded.manifest_digest, first.manifest.manifest_digest);

    let second = plan_content_repack(
        &source,
        Some(&first.manifest),
        input(),
        ContentRepackLimits::default(),
    )
    .unwrap();
    match second {
        ContentRepackPlan::NoOp {
            generation,
            fingerprint,
        } => {
            assert_eq!(generation, first.manifest.generation);
            assert_eq!(fingerprint, first.manifest.content_fingerprint);
        }
        ContentRepackPlan::Replace(_) | ContentRepackPlan::ReplacePrepared(_) => {
            panic!("identical representation must be a no-op")
        }
    }
}

#[test]
fn conflicting_lineage_parents_fall_back_to_full_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let target = source.put(&version(0)).unwrap();
    let left = source.put(&version(1)).unwrap();
    let right = source.put(&version(2)).unwrap();
    let input = |lineages| ContentRepackInput {
        content_ids: vec![target, left, right],
        lineages_newest_first: lineages,
    };
    let ContentRepackPlan::Replace(first) = plan_content_repack(
        &source,
        None,
        input(vec![vec![left, target], vec![right, target]]),
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    let ContentRepackPlan::Replace(second) = plan_content_repack(
        &source,
        None,
        input(vec![vec![right, target], vec![left, target]]),
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    let target_entry = first
        .manifest
        .entries
        .iter()
        .find(|entry| entry.id == target)
        .unwrap();
    assert_eq!(target_entry.delta_base, None);
    assert_eq!(target_entry.delta_depth, 0);
    assert_eq!(first.manifest, second.manifest);
}

#[test]
fn disk_backed_plan_streams_envelopes_without_retaining_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::with_durability(dir.path(), Durability::Deferred);
    let body = vec![b'x'; 32 * 1024];
    let ids = (0..2_000u64)
        .map(|value| {
            let mut bytes = body.clone();
            bytes[..8].copy_from_slice(&value.to_be_bytes());
            source.put(&bytes).unwrap()
        })
        .collect::<Vec<_>>();
    let ContentRepackPlan::Replace(analysis) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: ids,
            lineages_newest_first: Vec::new(),
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    let prepared = prepare_content_repack(&source, &analysis).unwrap();
    assert!(prepared.scratch_bytes() > 0);
    assert_eq!(
        prepared.envelopes().unwrap().map(Result::unwrap).count(),
        prepared.manifest.entries.len()
    );
}

#[test]
fn default_inventory_bound_covers_realistic_quarter_million_records() {
    let limits = ContentRepackLimits::default();
    assert_eq!(limits.max_content_ids, 250_000);

    // The limit is checked before any content read or scratch write. Distinct
    // synthetic IDs make this a scale/property test without creating 250k files.
    let mut content_ids = Vec::with_capacity(limits.max_content_ids + 1);
    for value in 0..=limits.max_content_ids {
        content_ids.push(ContentId::of(&(value as u64).to_be_bytes()));
    }
    let dir = tempfile::tempdir().unwrap();
    let error = plan_content_repack(
        &ContentStore::new(dir.path()),
        None,
        ContentRepackInput {
            content_ids,
            lineages_newest_first: Vec::new(),
        },
        limits,
    )
    .unwrap_err();
    assert!(error.to_string().contains("content IDs limit exceeded"));
}

#[derive(Default)]
struct RecordingTarget {
    events: Mutex<Vec<&'static str>>,
    fail_publish: bool,
}

impl ContentGenerationTarget for RecordingTarget {
    fn stage(&self, prepared: &PreparedContentGeneration) -> Result<(), String> {
        let mut count = 0;
        for envelope in prepared.envelopes().map_err(|error| error.to_string())? {
            envelope.map_err(|error| error.to_string())?;
            count += 1;
        }
        assert_eq!(count, prepared.manifest.entries.len());
        self.events.lock().unwrap().push("stage");
        Ok(())
    }
    fn publish(&self, _: Option<u64>, _: &ContentGenerationManifest) -> Result<(), String> {
        self.events.lock().unwrap().push("publish");
        if self.fail_publish {
            Err("injected".into())
        } else {
            Ok(())
        }
    }
    fn recover(&self) -> Result<(), String> {
        self.events.lock().unwrap().push("recover");
        Ok(())
    }
}

#[test]
fn integration_stages_before_atomic_publish_and_exposes_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let id = source.put(b"one").unwrap();
    let ContentRepackPlan::Replace(analysis) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: vec![id],
            lineages_newest_first: vec![],
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    let prepared = prepare_content_repack(&source, &analysis).unwrap();
    let target = RecordingTarget {
        fail_publish: true,
        ..Default::default()
    };
    assert!(publish_content_repack(&target, None, &prepared).is_err());
    target.recover().unwrap();
    assert_eq!(
        *target.events.lock().unwrap(),
        vec!["stage", "publish", "recover"]
    );
}

#[test]
fn limits_overflow_and_unknown_lineage_references_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let source = ContentStore::new(dir.path());
    let id = source.put(b"one").unwrap();
    let unknown = ContentId::of(b"unknown");
    assert!(
        plan_content_repack(
            &source,
            None,
            ContentRepackInput {
                content_ids: vec![id],
                lineages_newest_first: vec![vec![unknown]]
            },
            ContentRepackLimits::default(),
        )
        .is_err()
    );
    assert!(
        plan_content_repack(
            &source,
            None,
            ContentRepackInput {
                content_ids: vec![id],
                lineages_newest_first: vec![]
            },
            ContentRepackLimits {
                max_content_ids: 0,
                ..ContentRepackLimits::default()
            },
        )
        .is_err()
    );

    let ContentRepackPlan::Replace(first) = plan_content_repack(
        &source,
        None,
        ContentRepackInput {
            content_ids: vec![id],
            lineages_newest_first: vec![],
        },
        ContentRepackLimits::default(),
    )
    .unwrap() else {
        unreachable!()
    };
    let maximum_generation =
        ContentGenerationManifest::new(u64::MAX, first.manifest.entries).unwrap();
    assert!(matches!(
        plan_content_repack(
            &source,
            Some(&maximum_generation),
            ContentRepackInput {
                content_ids: vec![id, source.put(b"two").unwrap()],
                lineages_newest_first: vec![],
            },
            ContentRepackLimits::default(),
        ),
        Err(wheat_store::ContentRepackError::GenerationOverflow)
    ));
}

#[test]
#[ignore = "representative-scale manifest codec benchmark"]
fn benchmark_sharded_manifest_208k_entries() {
    use std::time::Instant;

    const ENTRIES: usize = 208_000;
    let entries = (0..ENTRIES)
        .map(|index| ContentGenerationEntry {
            id: ContentId::from_digest((index as u64).to_be_bytes().repeat(4).try_into().unwrap()),
            envelope_digest: ((index as u64).wrapping_mul(3))
                .to_be_bytes()
                .repeat(4)
                .try_into()
                .unwrap(),
            pack_offset: (index % CONTENT_GENERATION_SHARD_ENTRIES) as u64 * 128,
            physical_length: 128,
            logical_length: 256,
            delta_base: None,
            delta_depth: 0,
        })
        .collect();
    let started = Instant::now();
    let manifest = ContentGenerationManifest::new(1, entries).unwrap();
    let seal_elapsed = started.elapsed();

    let started = Instant::now();
    let index = manifest.index().unwrap().encoded().unwrap();
    let shards = (0..manifest.shard_count())
        .map(|shard| manifest.encode_shard(shard).unwrap())
        .collect::<Vec<_>>();
    let encode_elapsed = started.elapsed();
    let started = Instant::now();
    let decoded =
        ContentGenerationManifest::decode(&index, |shard| Ok(shards[shard].clone())).unwrap();
    let decode_elapsed = started.elapsed();
    let started = Instant::now();
    let one_shard = ContentGenerationIndex::decode(&index)
        .unwrap()
        .decode_shard(shards.len() / 2, &shards[shards.len() / 2])
        .unwrap();
    let one_shard_elapsed = started.elapsed();

    assert_eq!(decoded, manifest);
    assert_eq!(one_shard.len(), CONTENT_GENERATION_SHARD_ENTRIES);
    eprintln!(
        "208k content generation: index {} bytes, {} shards of {} bytes total; \
         seal {seal_elapsed:?}, encode {encode_elapsed:?}, decode {decode_elapsed:?}, \
         index + one shard {one_shard_elapsed:?}",
        index.len(),
        shards.len(),
        shards.iter().map(Vec::len).sum::<usize>(),
    );
}
