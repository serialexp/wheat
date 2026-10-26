//! The two layouts of a directory: a flat `TreeV1` of at most 256 entries, and a
//! chunked tree over a radix keyed by each name's hash.

use std::collections::{BTreeMap, HashMap};

use wheat_core::{
    CanonicalObject, CanonicalObjectError, CanonicalObjectKind, ChunkedTree, ContentId,
    DIRECTORY_CHUNK_BUCKET, DirectoryChunkChild, DirectoryChunkEntry, DirectoryChunkId,
    DirectoryChunkNode, DirectoryEntry, DirectoryNode, FLAT_DIRECTORY_MAX_ENTRIES, FileKind,
    FileValue, ObjectId, TreeId, build_directory, decode_chunked_tree, decode_directory_chunk_node,
    directory_name_key, validate_canonical_object,
};

fn file(name: &str) -> DirectoryEntry {
    DirectoryEntry::File(FileValue {
        content: ContentId::of(name.as_bytes()),
        kind: FileKind::Regular,
    })
}

fn entries(count: usize) -> BTreeMap<String, DirectoryEntry> {
    (0..count)
        .map(|index| {
            let name = format!("file-{index:05}.txt");
            let entry = if index % 7 == 0 {
                DirectoryEntry::Directory(TreeId::from_digest([index as u8; 32]))
            } else {
                file(&name)
            };
            (name, entry)
        })
        .collect()
}

fn build(entries: BTreeMap<String, DirectoryEntry>) -> (TreeId, HashMap<ObjectId, Vec<u8>>) {
    let mut objects = HashMap::new();
    let id = build_directory(entries, |object: CanonicalObject| {
        objects.insert(object.id, object.bytes);
    })
    .unwrap();
    (id, objects)
}

/// Every entry under `chunk`, checking that each sits at its name's key and that
/// only a group of more than [`DIRECTORY_CHUNK_BUCKET`] entries became a node.
fn collect(
    objects: &HashMap<ObjectId, Vec<u8>>,
    chunk: DirectoryChunkId,
    above: &mut Vec<u8>,
    out: &mut BTreeMap<String, DirectoryEntry>,
    nodes: &mut usize,
) {
    *nodes += 1;
    let node = decode_directory_chunk_node(&objects[&chunk.into()]).unwrap();
    if !above.is_empty() && node.children.is_empty() {
        assert!(node.entries.len() > DIRECTORY_CHUNK_BUCKET);
    }
    let start = above.len();
    above.extend_from_slice(&node.prefix);
    for (suffix, entry) in &node.entries {
        let key = [above.as_slice(), suffix].concat();
        assert_eq!(key, directory_name_key(&entry.name));
        assert!(out.insert(entry.name.clone(), entry.entry).is_none());
    }
    for child in &node.children {
        above.push(child.edge);
        collect(objects, child.child, above, out, nodes);
        above.pop();
    }
    above.truncate(start);
}

#[test]
fn a_directory_of_at_most_256_entries_is_flat() {
    let (id, objects) = build(entries(FLAT_DIRECTORY_MAX_ENTRIES));
    assert_eq!(objects.len(), 1);
    let bytes = &objects[&id.into()];
    assert_eq!(
        validate_canonical_object(bytes),
        Ok(CanonicalObjectKind::Tree)
    );
    assert_eq!(
        id,
        DirectoryNode::new(entries(FLAT_DIRECTORY_MAX_ENTRIES))
            .unwrap()
            .id()
    );
}

#[test]
fn a_wider_directory_is_chunked_and_holds_every_entry_at_its_key() {
    for count in [FLAT_DIRECTORY_MAX_ENTRIES + 1, 10_000] {
        let expected = entries(count);
        let (id, objects) = build(expected.clone());
        for bytes in objects.values() {
            validate_canonical_object(bytes).unwrap();
        }
        let root_bytes = &objects[&id.into()];
        assert_eq!(
            validate_canonical_object(root_bytes),
            Ok(CanonicalObjectKind::ChunkedTree)
        );
        let tree = decode_chunked_tree(root_bytes).unwrap();
        assert_eq!(tree.count, count as u64);
        let mut found = BTreeMap::new();
        let mut nodes = 0;
        collect(&objects, tree.root, &mut Vec::new(), &mut found, &mut nodes);
        assert_eq!(found, expected);
        // The chunked tree and its nodes are all the directory writes.
        assert_eq!(objects.len(), nodes + 1);
        // Buckets of two stay inline, so collisions on the first byte do not each
        // make a node: 257 entries take 18 nodes, where single-entry buckets
        // took 72.
        if count == FLAT_DIRECTORY_MAX_ENTRIES + 1 {
            assert_eq!(nodes, 18);
        }
    }
}

#[test]
fn a_chunked_tree_holds_more_than_256_entries() {
    let root = DirectoryChunkId::from_digest([7; 32]);
    assert!(matches!(
        ChunkedTree::new(FLAT_DIRECTORY_MAX_ENTRIES as u64, root),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    let tree = ChunkedTree::new(FLAT_DIRECTORY_MAX_ENTRIES as u64 + 1, root).unwrap();
    let bytes = tree.canonical_bytes();
    // The wire tag, the count 257 as a two-byte varint, the root.
    assert_eq!(&bytes[..4], &[0x15, 0x01, 0x81, 0x02]);
    assert_eq!(bytes.len(), 4 + 32);
    assert_eq!(decode_chunked_tree(&bytes).unwrap(), tree);
    // The same bytes with the count lowered to 256 are not a chunked tree.
    let flat_count = [&[0x15, 0x01, 0x80, 0x02][..], &bytes[4..]].concat();
    assert!(decode_chunked_tree(&flat_count).is_err());
}

fn entry(name: &str, depth: usize) -> (Vec<u8>, DirectoryChunkEntry) {
    (
        directory_name_key(name)[depth..].to_vec(),
        DirectoryChunkEntry {
            name: name.to_owned(),
            entry: file(name),
        },
    )
}

#[test]
fn chunk_nodes_round_trip_and_refuse_entries_off_their_key() {
    let node = DirectoryChunkNode::new(
        Vec::new(),
        vec![DirectoryChunkChild {
            edge: directory_name_key("c")[0] ^ 0x80,
            child: DirectoryChunkId::from_digest([3; 32]),
        }],
        vec![entry("a", 0), entry("b", 0)],
    );
    // Distinct first bytes are what this test relies on.
    let first = |name: &str| directory_name_key(name)[0];
    assert_ne!(first("a"), first("b"));
    let node = node.unwrap();
    let bytes = node.canonical_bytes();
    assert_eq!(
        validate_canonical_object(&bytes),
        Ok(CanonicalObjectKind::DirectoryChunkNode)
    );
    assert_eq!(decode_directory_chunk_node(&bytes).unwrap(), node);

    // An entry filed under another name's key.
    let (_, b) = entry("b", 0);
    let misfiled = (directory_name_key("a")[..].to_vec(), b);
    assert!(matches!(
        DirectoryChunkNode::new(Vec::new(), Vec::new(), vec![misfiled, entry("c", 0)]),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    // A name no directory may hold.
    let (suffix, mut bad) = entry("..", 0);
    bad.name = "..".to_owned();
    assert!(matches!(
        DirectoryChunkNode::new(Vec::new(), Vec::new(), vec![(suffix, bad), entry("c", 0)]),
        Err(CanonicalObjectError::InvalidDirectoryComponent(_))
    ));
    // One branch is never canonical: it would be inlined in its parent.
    assert!(matches!(
        DirectoryChunkNode::new(Vec::new(), Vec::new(), vec![entry("a", 0)]),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    // A suffix runs to the end of the key: one cut short is refused.
    let (suffix, short) = entry("a", 0);
    let truncated = (suffix[..31].to_vec(), short);
    assert!(
        DirectoryChunkNode::new(Vec::new(), Vec::new(), vec![truncated, entry("b", 0)]).is_err()
    );
}

#[test]
fn the_layout_follows_the_entry_count_alone() {
    // Removing the 257th entry gives back exactly the flat directory of the rest.
    let mut wide = entries(FLAT_DIRECTORY_MAX_ENTRIES + 1);
    let (wide_id, _) = build(wide.clone());
    let last = wide.keys().next_back().unwrap().clone();
    wide.remove(&last);
    let (narrow_id, objects) = build(wide.clone());
    assert_ne!(wide_id, narrow_id);
    assert_eq!(
        validate_canonical_object(&objects[&narrow_id.into()]),
        Ok(CanonicalObjectKind::Tree)
    );
    assert_eq!(narrow_id, DirectoryNode::new(wide).unwrap().id());
}

#[test]
fn a_bucket_holds_at_most_two_entries_inline() {
    // Three names whose keys share their first byte.
    let mut by_first: HashMap<u8, Vec<String>> = HashMap::new();
    let names = (0..)
        .map(|index| format!("n{index}"))
        .find_map(|name| {
            let group = by_first.entry(directory_name_key(&name)[0]).or_default();
            group.push(name);
            (group.len() == DIRECTORY_CHUNK_BUCKET + 1).then(|| group.clone())
        })
        .unwrap();
    let other = (0..)
        .map(|index| format!("o{index}"))
        .find(|name| directory_name_key(name)[0] != directory_name_key(&names[0])[0])
        .unwrap();
    let bucket = |count: usize| {
        let mut entries: Vec<_> = names[..count].iter().map(|name| entry(name, 0)).collect();
        entries.push(entry(&other, 0));
        DirectoryChunkNode::new(Vec::new(), Vec::new(), entries)
    };
    assert!(bucket(DIRECTORY_CHUNK_BUCKET).is_ok());
    assert!(matches!(
        bucket(DIRECTORY_CHUNK_BUCKET + 1),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    // Nor may a bucket share its byte with a child.
    let shared = DirectoryChunkNode::new(
        Vec::new(),
        vec![DirectoryChunkChild {
            edge: directory_name_key(&names[0])[0],
            child: DirectoryChunkId::from_digest([3; 32]),
        }],
        vec![entry(&names[0], 0), entry(&other, 0)],
    );
    assert!(matches!(
        shared,
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
}
