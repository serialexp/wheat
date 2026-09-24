//! Measurement of Phase 1 (collection compaction) impact on repository size.
//!
//! Builds a 50-change benchmark repository and reports:
//! - Total object count and byte size
//! - Object count and byte size by wire tag
//! - Trie node count (the target of Phase 1 reduction)
//!
//! Run: cargo test -p wheat-store --test phase1_measurement -- --nocapture --ignored

use std::collections::BTreeMap;

use wheat_core::*;

const CHANGES: usize = 50;

fn exact(path: &str, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(replacement),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

/// Classify an object by its 2-byte big-endian wire tag (WheatObject discriminator).
fn wire_tag_name(bytes: &[u8]) -> &'static str {
    if bytes.len() < 2 {
        return "Unknown";
    }
    let tag = u16::from_be_bytes([bytes[0], bytes[1]]);
    match tag {
        0x0101 => "Tree",
        0x0201 => "ChangeObject",
        0x0301 => "Revision",
        0x0401 => "Application",
        0x0501 => "Conflict",
        0x0601 => "View",
        0x0701 => "ContentManifest",
        0x0801 => "RepositoryState",
        0x0901 => "RevisionMapNode",
        0x0a01 => "ApplicationMapNode",
        0x0b01 => "ConflictSetNode",
        0x0c02 => "ContentSetSnapshot",
        0x0c03 => "ContentSetDelta",
        0x0d01 => "RetainedSetNode",
        0x0e01 => "ViewLabelManifest",
        0x0f01 => "Envelope",
        0x1001 => "Edit",
        0x1101 => "ChangeMapNode",
        0x1201 => "ViewLineageMapNode",
        _ => "Unknown",
    }
}

fn is_trie_node(name: &str) -> bool {
    matches!(
        name,
        "RevisionMapNode"
            | "ChangeMapNode"
            | "RetainedSetNode"
            | "ConflictSetNode"
            | "ApplicationMapNode"
            | "ViewLineageMapNode"
    )
}

fn is_content_set_blob(name: &str) -> bool {
    matches!(name, "ContentSetSnapshot" | "ContentSetDelta")
}

#[test]
#[ignore = "measurement; run explicitly with --nocapture --ignored"]
fn measure_50_change_repository_closure() {
    let content_store =
        wheat_store::ContentStore::new(tempfile::tempdir().unwrap().path().join("content"));
    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    let mut effective = basis;
    let mut previous = None;

    for index in 0..CHANGES {
        let change = ChangeId::from_bytes((index as u128 + 1).to_be_bytes());
        let value = format!("// file-{index}: line of source code\n");
        content_store.put(value.as_bytes()).unwrap();
        let edits = exact(&format!("src/file_{index:03}.rs"), value.as_bytes());
        let outcome = repository
            .create_change_with_id(
                basis,
                effective,
                change,
                previous,
                edits,
                ApplicationContext::new(&content_store),
            )
            .unwrap();
        effective = outcome.resulting_view;
        previous = Some(change);
    }

    let closure = repository.canonical_closure();
    let mut by_kind: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut total_objects = 0usize;
    let mut total_bytes = 0usize;

    for object in &closure.objects {
        total_objects += 1;
        total_bytes += object.bytes.len();
        let kind = wire_tag_name(&object.bytes);
        let entry = by_kind.entry(kind).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += object.bytes.len();
    }

    println!();
    println!("============================================================");
    println!("  Wheat 50-change benchmark repository closure");
    println!("============================================================");
    println!();
    println!("  Total objects: {total_objects}");
    println!(
        "  Total bytes:   {total_bytes} ({:.1} KiB)",
        total_bytes as f64 / 1024.0
    );
    println!();
    println!(
        "  {:<25} {:>8} {:>10} {:>8}",
        "Kind", "Count", "Bytes", "Avg"
    );
    println!(
        "  {:<25} {:>8} {:>10} {:>8}",
        "-------------------------", "--------", "----------", "--------"
    );
    let mut kinds: Vec<_> = by_kind.iter().collect();
    kinds.sort_by_key(|b| std::cmp::Reverse(b.1.1));
    for (kind, (count, bytes)) in &kinds {
        println!(
            "  {:<25} {:>8} {:>10} {:>8}",
            kind,
            count,
            bytes,
            if *count > 0 { bytes / count } else { 0 }
        );
    }

    let trie_count: usize = by_kind
        .iter()
        .filter(|(k, _)| is_trie_node(k))
        .map(|(_, (c, _))| c)
        .sum();
    let trie_bytes: usize = by_kind
        .iter()
        .filter(|(k, _)| is_trie_node(k))
        .map(|(_, (_, b))| b)
        .sum();
    let non_trie_count = total_objects - trie_count;
    let non_trie_bytes = total_bytes - trie_bytes;
    println!();
    println!(
        "  Trie nodes:     {:>5} objects, {:>8} bytes ({:.1}%)",
        trie_count,
        trie_bytes,
        trie_bytes as f64 / total_bytes as f64 * 100.0
    );
    println!(
        "  Non-trie:       {:>5} objects, {:>8} bytes ({:.1}%)",
        non_trie_count,
        non_trie_bytes,
        non_trie_bytes as f64 / total_bytes as f64 * 100.0
    );

    let content_blob_count: usize = by_kind
        .iter()
        .filter(|(k, _)| is_content_set_blob(k))
        .map(|(_, (c, _))| c)
        .sum();
    let content_blob_bytes: usize = by_kind
        .iter()
        .filter(|(k, _)| is_content_set_blob(k))
        .map(|(_, (_, b))| b)
        .sum();
    println!(
        "  Content set blobs: {:>3} objects, {:>8} bytes ({:.1}%)",
        content_blob_count,
        content_blob_bytes,
        content_blob_bytes as f64 / total_bytes as f64 * 100.0
    );

    // Per-object overhead estimate: ~60 bytes index entry per object in a segment.
    let index_overhead = total_objects * 60;
    println!();
    println!(
        "  Estimated segment index overhead: {index_overhead} bytes ({:.1} KiB)",
        index_overhead as f64 / 1024.0
    );
    println!(
        "  Canonical + index total: {} bytes ({:.1} KiB)",
        total_bytes + index_overhead,
        (total_bytes + index_overhead) as f64 / 1024.0
    );

    // --- Block compression measurement ---
    // Build a segment from all closure objects and measure the actual physical
    // size with block compression enabled (codec=4 for small metadata records).
    use wheat_store::packed_segment_test_helpers::build_segment_bytes;
    let segment_bytes = build_segment_bytes(&closure.objects);
    let segment_len = segment_bytes.len();
    println!();
    println!("  --- Block compression measurement ---");
    println!(
        "  Segment total:   {segment_len} bytes ({:.1} KiB)",
        segment_len as f64 / 1024.0
    );
    println!(
        "  vs canonical:    {total_bytes} bytes ({:.1} KiB)",
        total_bytes as f64 / 1024.0
    );
    let ratio = segment_len as f64 / total_bytes as f64;
    println!("  Storage ratio:   {ratio:.2}x");
    println!(
        "  Savings from block compression: {:.1}%",
        (1.0 - (segment_len as f64 / (total_bytes + index_overhead) as f64)) * 100.0
    );
    println!();
}
