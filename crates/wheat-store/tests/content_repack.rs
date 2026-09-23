use std::sync::Mutex;

use wheat_core::ContentId;
use wheat_store::{
    ContentGenerationManifest, ContentGenerationTarget, ContentRepackInput, ContentRepackLimits,
    ContentRepackPlan, ContentStore, Durability, PreparedContentGeneration, plan_content_repack,
    prepare_content_repack, publish_content_repack,
};

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
    let serialized = serde_json::to_vec(&prepared.manifest).unwrap();
    let decoded: ContentGenerationManifest = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(decoded, prepared.manifest);
    let orphan_entry = prepared
        .manifest
        .entries
        .iter()
        .find(|entry| entry.id == orphan)
        .unwrap();
    assert_eq!(orphan_entry.delta_base, None);
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
    let mut maximum_generation = first.manifest;
    maximum_generation.generation = u64::MAX;
    maximum_generation.manifest_digest = manifest_digest(&maximum_generation);
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

fn manifest_digest(manifest: &ContentGenerationManifest) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"wheat-content-generation-v1\0");
    hash.update(&manifest.generation.to_be_bytes());
    hash.update(&manifest.content_fingerprint);
    hash.update(&(manifest.entries.len() as u64).to_be_bytes());
    for entry in &manifest.entries {
        hash.update(entry.id.as_bytes());
        hash.update(&entry.envelope_digest);
        hash.update(&entry.pack_offset.to_be_bytes());
        hash.update(&entry.physical_length.to_be_bytes());
        hash.update(&entry.logical_length.to_be_bytes());
        if let Some(base) = entry.delta_base {
            hash.update(&[1]);
            hash.update(base.as_bytes());
        } else {
            hash.update(&[0]);
        }
        hash.update(&[entry.delta_depth]);
    }
    *hash.finalize().as_bytes()
}
