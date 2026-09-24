use std::fs;

use wheat_core::ContentId;
use wheat_store::{ContentStore, ContentStoreError};

// ---------------------------------------------------------------------------
// Delta compression tests
// ---------------------------------------------------------------------------

#[test]
fn delta_round_trip_through_content_store() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Simulate a typical source file edit: one line changed in a multi-KB file.
    let mut base_content = String::new();
    for i in 0..100 {
        base_content.push_str(&format!("fn function_{i}() -> i32 {{ {i} }}\n"));
    }
    let target_content = base_content.replace(
        "fn function_50() -> i32 { 50 }",
        "fn function_50() -> i32 { 42 }",
    );

    let base_bytes = base_content.as_bytes();
    let target_bytes = target_content.as_bytes();

    // Store the base first (like a previous commit).
    let base_id = store.put(base_bytes).unwrap();

    // Store the target with delta hint.
    let target_id = ContentId::of(target_bytes);
    store
        .put_with_base(target_id, target_bytes, base_id)
        .unwrap();

    // Read it back — should reconstruct correctly through delta resolution.
    let read_back = store.get(target_id).unwrap();
    assert_eq!(read_back, target_bytes);

    // Verify it was actually stored as a delta (not raw) by checking the
    // on-disk file size. The delta should be much smaller than the original.
    let target_path = store.path(target_id);
    let file_size = fs::metadata(&target_path).unwrap().len();
    assert!(
        file_size < target_bytes.len() as u64 / 2,
        "delta file ({file_size} bytes) should be much smaller than raw ({} bytes)",
        target_bytes.len()
    );
}

#[test]
fn delta_chain_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Build a chain of 3 versions (v1 → v2 delta → v3 delta).
    let mut v1 = String::new();
    for i in 0..50 {
        v1.push_str(&format!("line {i}: original content\n"));
    }
    let v1_bytes = v1.as_bytes();
    let v1_id = store.put(v1_bytes).unwrap();

    let v2 = v1.replace("line 10: original content", "line 10: modified in v2");
    let v2_bytes = v2.as_bytes();
    let v2_id = ContentId::of(v2_bytes);
    store.put_with_base(v2_id, v2_bytes, v1_id).unwrap();

    let v3 = v2.replace("line 20: original content", "line 20: modified in v3");
    let v3_bytes = v3.as_bytes();
    let v3_id = ContentId::of(v3_bytes);
    store.put_with_base(v3_id, v3_bytes, v2_id).unwrap();

    // Read all versions — they should all reconstruct correctly.
    assert_eq!(store.get(v1_id).unwrap(), v1_bytes);
    assert_eq!(store.get(v2_id).unwrap(), v2_bytes);
    assert_eq!(store.get(v3_id).unwrap(), v3_bytes);
}

#[test]
fn sixteen_link_delta_chain_resolves_and_seventeenth_falls_back_full() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let mut bytes = (0..8 * 1024)
        .map(|offset| ((offset * 131 + offset / 7) % 251) as u8)
        .collect::<Vec<_>>();
    let mut previous = store.put(&bytes).unwrap();
    let mut ids = vec![previous];
    for version in 1u64..=17 {
        bytes[100..108].copy_from_slice(&version.to_be_bytes());
        let id = ContentId::of(&bytes);
        store.put_with_base(id, &bytes, previous).unwrap();
        ids.push(id);
        previous = id;
    }
    for id in ids {
        assert!(store.get(id).is_ok());
    }
    let comparison_directory = tempfile::tempdir().unwrap();
    let full_store = ContentStore::new(comparison_directory.path());
    let full_id = full_store.put(&bytes).unwrap();
    assert_eq!(
        fs::metadata(store.path(previous)).unwrap().len(),
        fs::metadata(full_store.path(full_id)).unwrap().len()
    );
}

#[test]
fn delta_falls_back_to_raw_for_unrelated_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Store base content.
    let base_bytes = b"hello world this is the base content for testing";
    let base_id = store.put(base_bytes).unwrap();

    // Store completely different content with the base as a "hint".
    // The delta would be larger than the raw content, so it should
    // fall back to raw storage.
    let target_bytes = b"completely different content, nothing in common at all xyz";
    let target_id = ContentId::of(target_bytes);
    store
        .put_with_base(target_id, target_bytes, base_id)
        .unwrap();

    // Should still read back correctly.
    assert_eq!(store.get(target_id).unwrap(), target_bytes);
}

/// A delta envelope's header records the reconstructed length, so sizing it never
/// resolves the base: the length still comes back with the base gone.
#[test]
fn logical_length_of_a_delta_reads_only_its_header() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let mut base = String::new();
    for i in 0..100 {
        base.push_str(&format!("fn function_{i}() -> i32 {{ {i} }}\n"));
    }
    let target = format!("{base}fn extra() {{}}\n");
    let base_id = store.put(base.as_bytes()).unwrap();
    let target_id = ContentId::of(target.as_bytes());
    store
        .put_with_base(target_id, target.as_bytes(), base_id)
        .unwrap();
    assert!(fs::metadata(store.path(target_id)).unwrap().len() < target.len() as u64 / 2);

    fs::remove_file(store.path(base_id)).unwrap();
    assert_eq!(
        store.logical_length(target_id).unwrap(),
        target.len() as u64
    );
    assert!(store.get(target_id).is_err());

    let missing = ContentId::of(b"never stored");
    assert!(matches!(
        store.logical_length(missing),
        Err(ContentStoreError::Missing(actual)) if actual == missing
    ));
}

#[test]
fn delta_falls_back_when_base_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Try to store with a base that doesn't exist.
    let fake_base_id = ContentId::of(b"nonexistent base");
    let target_bytes = b"some target content that should be stored raw";
    let target_id = ContentId::of(target_bytes);
    store
        .put_with_base(target_id, target_bytes, fake_base_id)
        .unwrap();

    // Should store raw and still read back correctly.
    assert_eq!(store.get(target_id).unwrap(), target_bytes);
}

#[test]
fn delta_bounded_read_applies_max_to_final_output() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Create a file and a slightly larger version.
    let mut base = String::new();
    for i in 0..100 {
        base.push_str(&format!("line {i}\n"));
    }
    let target = format!("{base}extra line at the end\n");

    let base_id = store.put(base.as_bytes()).unwrap();
    let target_id = ContentId::of(target.as_bytes());
    store
        .put_with_base(target_id, target.as_bytes(), base_id)
        .unwrap();

    // Bounded read with max < target size should fail.
    assert!(matches!(
        store.get_bounded(target_id, target.len() - 1),
        Err(ContentStoreError::TooLarge { .. })
    ));

    // Bounded read with max = target size should succeed.
    assert_eq!(
        store.get_bounded(target_id, target.len()).unwrap(),
        target.as_bytes()
    );
}

#[test]
fn delta_base_larger_than_target() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Base is large, target is truncated (smaller).
    let mut base = String::new();
    for i in 0..200 {
        base.push_str(&format!("line {i}: padding content here\n"));
    }
    let target = base[..base.len() / 2].to_string();

    let base_id = store.put(base.as_bytes()).unwrap();
    let target_id = ContentId::of(target.as_bytes());
    store
        .put_with_base(target_id, target.as_bytes(), base_id)
        .unwrap();

    // Should reconstruct correctly even though the base is larger.
    // The max bound (target.len()) should work since we only apply
    // max to the final output, not to the base fetch.
    assert_eq!(
        store.get_bounded(target_id, target.len()).unwrap(),
        target.as_bytes()
    );
}

#[test]
fn put_many_with_bases_mixes_delta_and_raw() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    // Store base content.
    let mut base = String::new();
    for i in 0..100 {
        base.push_str(&format!("function_{i}()\n"));
    }
    let base_id = store.put(base.as_bytes()).unwrap();

    // Prepare two items: one with a delta base, one raw.
    let modified = base.replace("function_50()", "function_50_v2()");
    let modified_id = ContentId::of(modified.as_bytes());
    let raw_bytes = b"brand new file with no base";
    let raw_id = ContentId::of(raw_bytes.as_slice());

    store
        .put_many_with_bases(
            vec![
                (modified_id, modified.as_bytes().to_vec(), Some(base_id)),
                (raw_id, raw_bytes.to_vec(), None),
            ],
            4,
        )
        .unwrap();

    assert_eq!(store.get(modified_id).unwrap(), modified.as_bytes());
    assert_eq!(store.get(raw_id).unwrap(), raw_bytes);
}

#[test]
fn retains_reads_and_deduplicates_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());

    let first = store.put(b"same bytes").unwrap();
    let second = store.put(b"same bytes").unwrap();

    assert_eq!(first, second);
    assert_eq!(store.get(first).unwrap(), b"same bytes");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn creates_a_missing_store_root_and_fanout_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("content");
    let store = ContentStore::new(&root);

    let id = store.put(b"durable directory entries").unwrap();

    assert!(root.is_dir());
    assert!(store.path(id).parent().unwrap().is_dir());
    assert_eq!(store.get(id).unwrap(), b"durable directory entries");
    assert_ne!(
        fs::read(store.path(id)).unwrap(),
        b"durable directory entries"
    );
}

#[test]
fn retry_verifies_an_already_published_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("content");
    let store = ContentStore::new(&root);
    let bytes = b"published before a directory sync failure";
    let id = ContentId::of(bytes);
    let path = store.path(id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // First publication may fail after rename; produce its definitive bytes via
    // another store rather than constructing the private envelope in this test.
    let staging = ContentStore::new(directory.path().join("staging"));
    staging.put(bytes).unwrap();
    fs::copy(staging.path(id), &path).unwrap();

    assert_eq!(store.put(bytes).unwrap(), id);
    assert_eq!(store.get(id).unwrap(), bytes);
}

#[test]
fn distinguishes_missing_and_corrupt_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let missing = ContentId::of(b"missing");
    assert!(matches!(store.get(missing), Err(ContentStoreError::Missing(id)) if id == missing));

    let stored = store.put(b"original").unwrap();
    fs::write(store.path(stored), b"corrupt").unwrap();
    assert!(matches!(
        store.get(stored),
        Err(ContentStoreError::InvalidFormat { id, .. }) if id == stored
    ));
}

#[test]
fn bounded_read_rejects_loose_content_before_loading_it() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = vec![7; 64 * 1024];
    let id = store.put(&bytes).unwrap();

    assert!(matches!(
        store.get_bounded(id, bytes.len() - 1),
        Err(ContentStoreError::TooLarge { id: actual, max, minimum })
            if actual == id && max == bytes.len() - 1 && minimum == bytes.len()
    ));
    assert_eq!(store.get_bounded(id, bytes.len()).unwrap(), bytes);
}

#[test]
fn bounded_read_still_reports_missing_and_corrupt_loose_content() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let missing = ContentId::of(b"missing bounded");
    assert!(matches!(
        store.get_bounded(missing, 32),
        Err(ContentStoreError::Missing(id)) if id == missing
    ));

    let id = store.put(b"original bounded").unwrap();
    fs::write(store.path(id), b"different bounded").unwrap();
    assert!(matches!(
        store.get_bounded(id, 32),
        Err(ContentStoreError::InvalidFormat { id: actual, .. }) if actual == id
    ));
}

#[test]
fn batch_retention_is_parallel_deduplicated_and_integrity_checked() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let first = ContentId::of(b"first");
    let second = ContentId::of(b"second");

    store
        .put_many(
            [
                (first, b"first".to_vec()),
                (second, b"second".to_vec()),
                (first, b"first".to_vec()),
            ],
            4,
        )
        .unwrap();
    assert_eq!(store.get(first).unwrap(), b"first");
    assert_eq!(store.get(second).unwrap(), b"second");

    let error = store
        .put_many([(first, b"not first".to_vec())], 2)
        .unwrap_err();
    assert!(matches!(
        error,
        ContentStoreError::Corrupt { expected, .. } if expected == first
    ));
}
