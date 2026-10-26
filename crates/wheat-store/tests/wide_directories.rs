//! Every tree reader sees a wide (chunked) directory as the entries it holds, and a
//! name lookup or a diff reads only the chunks it needs.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use object_store::memory::InMemory;
use wheat_core::{
    CanonicalObject, ChunkedTree, ContentId, DirectoryChunkChild, DirectoryChunkNode,
    DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId, TreeId, build_directory,
    decode_chunked_tree, decode_directory_chunk_node,
};
use wheat_store::{
    ImmutableObjectStore, MerkleTreeError, MerkleTreeStore, ObjectStoreAdapter, ObjectStoreError,
    StoredObject, TreeContentWalkLimits, TreeDiff, TreeDiffLimits, TreeFile, TreeListing,
    TreePathLimits, TreePathSelection, TreeSelectionLimits, TreeWalkLimits,
};

struct CountingStore<S> {
    inner: S,
    reads: AtomicUsize,
}

impl<S> CountingStore<S> {
    fn reads(&self) -> usize {
        self.reads.swap(0, Ordering::Relaxed)
    }
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for CountingStore<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

type Store = CountingStore<ObjectStoreAdapter>;

fn store() -> Store {
    CountingStore {
        inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "wide-directories"),
        reads: AtomicUsize::new(0),
    }
}

fn value(name: &str, version: u8) -> FileValue {
    FileValue {
        content: ContentId::of(&[name.as_bytes(), &[version]].concat()),
        kind: FileKind::Regular,
    }
}

fn name(index: usize) -> String {
    format!("f{index:05}.txt")
}

fn files(count: usize, version: u8) -> BTreeMap<String, DirectoryEntry> {
    (0..count)
        .map(|index| {
            let name = name(index);
            let entry = DirectoryEntry::File(value(&name, version));
            (name, entry)
        })
        .collect()
}

/// Writes `entries` as `build_directory` lays them out; returns the tree and how
/// many objects it took.
async fn put_directory(
    store: &Store,
    entries: BTreeMap<String, DirectoryEntry>,
) -> (TreeId, usize) {
    let mut objects = Vec::new();
    let id = build_directory(entries, |object: CanonicalObject| objects.push(object)).unwrap();
    for object in &objects {
        store.put_if_absent(object.id, &object.bytes).await.unwrap();
    }
    (id, objects.len())
}

async fn put_node(store: &Store, entries: BTreeMap<String, DirectoryEntry>) -> TreeId {
    MerkleTreeStore::new(store)
        .write_node(&DirectoryNode::new(entries).unwrap())
        .await
        .unwrap()
}

/// A root holding `wide`, a directory of `count` files and a subdirectory `sub`, and
/// a file `z` after it. Returns the root, `wide`, and how many objects `wide` took.
async fn fixture(store: &Store, count: usize, version: u8) -> (TreeId, TreeId, usize) {
    let sub = put_node(
        store,
        BTreeMap::from([("a".to_owned(), DirectoryEntry::File(value("a", 0)))]),
    )
    .await;
    let mut wide = files(count, version);
    wide.insert("sub".to_owned(), DirectoryEntry::Directory(sub));
    let (wide, objects) = put_directory(store, wide).await;
    let root = put_node(
        store,
        BTreeMap::from([
            ("wide".to_owned(), DirectoryEntry::Directory(wide)),
            ("z".to_owned(), DirectoryEntry::File(value("z", 0))),
        ]),
    )
    .await;
    (root, wide, objects)
}

fn expected_files(count: usize, version: u8) -> Vec<TreeFile> {
    let mut expected: Vec<_> = (0..count)
        .map(|index| TreeFile {
            path: format!("wide/{}", name(index)),
            value: value(&name(index), version),
        })
        .collect();
    expected.push(TreeFile {
        path: "wide/sub/a".to_owned(),
        value: value("a", 0),
    });
    expected.push(TreeFile {
        path: "z".to_owned(),
        value: value("z", 0),
    });
    expected
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[tokio::test]
async fn every_reader_sees_a_wide_directory_as_its_entries() {
    let store = store();
    let (root, wide, _) = fixture(&store, 1_000, 0).await;
    let trees = MerkleTreeStore::new(&store);
    let expected = expected_files(1_000, 0);

    assert_eq!(
        trees
            .walk_files(root, TreeWalkLimits::default())
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        trees
            .walk_path(root, &path(&["wide"]), TreePathLimits::default())
            .await
            .unwrap(),
        TreePathSelection::Directory(expected[..1_001].to_vec())
    );
    assert_eq!(
        trees
            .walk_path(
                root,
                &path(&["wide", "f00042.txt"]),
                TreePathLimits::default()
            )
            .await
            .unwrap(),
        TreePathSelection::File(expected[42].clone())
    );
    assert_eq!(
        trees
            .select_paths(
                root,
                &["wide/f00010.txt".to_owned(), "wide/sub".to_owned()],
                TreeSelectionLimits::default()
            )
            .await
            .unwrap(),
        [expected[10].clone(), expected[1_000].clone()]
    );
    let mut cursor = trees
        .content_cursor(root, TreeContentWalkLimits::default())
        .await
        .unwrap();
    let mut values = Vec::new();
    while let Some(value) = cursor.next().await.unwrap() {
        values.push(value);
    }
    assert_eq!(
        values,
        expected.iter().map(|file| file.value).collect::<Vec<_>>()
    );

    assert_eq!(
        trees
            .lookup(root, &path(&["wide", "f00500.txt"]))
            .await
            .unwrap(),
        Some(expected[500].value)
    );
    assert_eq!(
        trees
            .lookup(root, &path(&["wide", "sub", "a"]))
            .await
            .unwrap(),
        Some(value("a", 0))
    );
    assert_eq!(
        trees
            .lookup(root, &path(&["wide", "absent"]))
            .await
            .unwrap(),
        None
    );
    let listing = trees
        .list_directory(
            root,
            &path(&["wide"]),
            Some("f00997.txt"),
            NonZeroUsize::new(2).unwrap(),
        )
        .await
        .unwrap();
    let TreeListing::Directory(page) = listing else {
        panic!("wide is a directory: {listing:?}");
    };
    assert_eq!(
        page.entries
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["f00998.txt", "f00999.txt"]
    );
    assert_eq!(page.next.as_deref(), Some("f00999.txt"));
    let node = trees.read_node(wide).await.unwrap();
    assert_eq!(node.entries.len(), 1_001);
}

#[tokio::test]
async fn lookups_read_only_the_chunks_on_their_path() {
    let store = store();
    let (root, _, objects) = fixture(&store, 10_000, 0).await;
    let trees = MerkleTreeStore::new(&store);
    store.reads();

    trees
        .lookup(root, &path(&["wide", "f04242.txt"]))
        .await
        .unwrap()
        .unwrap();
    // The root, the header, and the chunks on one key's path.
    let one = store.reads();
    assert!(one <= 2 + 3, "one lookup read {one} objects");

    // A sorted batch reads each chunk at most once: here every name, so every chunk
    // exactly once, besides the root and the header.
    let names: Vec<_> = (0..10_000)
        .map(|index| format!("wide/{}", name(index)))
        .collect();
    let paths: Vec<_> = names.iter().map(String::as_str).collect();
    let found = trees.lookup_many(root, &paths).await.unwrap();
    assert!(found.iter().all(Option::is_some));
    assert_eq!(store.reads(), 1 + objects);
}

#[tokio::test]
async fn a_diff_of_two_wide_directories_reads_only_where_they_differ() {
    let store = store();
    let (old, _, objects) = fixture(&store, 1_000, 0).await;
    let old_root = MerkleTreeStore::new(&store).read_node(old).await.unwrap();
    let DirectoryEntry::Directory(old_wide) = old_root.entries["wide"] else {
        panic!("wide is a directory");
    };
    let mut wide = MerkleTreeStore::new(&store)
        .read_node(old_wide)
        .await
        .unwrap()
        .entries;
    let changed = [3, 500, 998];
    for index in changed {
        wide.insert(name(index), DirectoryEntry::File(value(&name(index), 1)));
    }
    wide.remove(&name(7));
    wide.insert(name(1_000), DirectoryEntry::File(value(&name(1_000), 0)));
    let (new_wide, _) = put_directory(&store, wide).await;
    let new = put_node(
        &store,
        BTreeMap::from([
            ("wide".to_owned(), DirectoryEntry::Directory(new_wide)),
            ("z".to_owned(), DirectoryEntry::File(value("z", 0))),
        ]),
    )
    .await;
    store.reads();

    let trees = MerkleTreeStore::new(&store);
    let mut cursor = trees.diff_cursor(old, new, TreeDiffLimits::default());
    let mut diffs = Vec::new();
    while let Some(diff) = cursor.next().await.unwrap() {
        diffs.push(diff);
    }
    let changed_file = |index: usize| TreeDiff {
        path: format!("wide/{}", name(index)),
        old: Some(value(&name(index), 0)),
        new: Some(value(&name(index), 1)),
    };
    assert_eq!(
        diffs,
        [
            changed_file(3),
            TreeDiff {
                path: format!("wide/{}", name(7)),
                old: Some(value(&name(7), 0)),
                new: None,
            },
            changed_file(500),
            changed_file(998),
            TreeDiff {
                path: format!("wide/{}", name(1_000)),
                old: None,
                new: Some(value(&name(1_000), 0)),
            },
        ]
    );
    // Two roots and two headers, then at most a root-to-entry chunk path on each
    // side per difference; reading either directory whole takes `objects`.
    let reads = store.reads();
    let bound = 4 + 2 * 5 * 3;
    assert!(reads <= bound, "the diff read {reads} objects");
    assert!(bound < objects);
}

#[tokio::test]
async fn a_diff_across_the_flat_threshold_lists_the_change() {
    let store = store();
    let (flat, _, flat_objects) = fixture(&store, 255, 0).await;
    let (wide, _, wide_objects) = fixture(&store, 256, 0).await;
    // With `sub`, 256 entries are flat and 257 chunked.
    assert_eq!(flat_objects, 1);
    assert!(wide_objects > 1);
    let trees = MerkleTreeStore::new(&store);
    let added = TreeDiff {
        path: format!("wide/{}", name(255)),
        old: None,
        new: Some(value(&name(255), 0)),
    };
    for (old, new, expected) in [
        (flat, wide, added.clone()),
        (
            wide,
            flat,
            TreeDiff {
                old: added.new,
                new: None,
                ..added
            },
        ),
    ] {
        let mut cursor = trees.diff_cursor(old, new, TreeDiffLimits::default());
        assert_eq!(cursor.next().await.unwrap(), Some(expected));
        assert_eq!(cursor.next().await.unwrap(), None);
    }
}

async fn chunked_header(store: &Store, wide: TreeId) -> ChunkedTree {
    let object = store.get(wide.into()).await.unwrap();
    decode_chunked_tree(&object.canonical_bytes).unwrap()
}

async fn root_with(store: &Store, header: ChunkedTree) -> TreeId {
    let wide = header.id();
    store
        .put_if_absent(wide.into(), &header.canonical_bytes())
        .await
        .unwrap();
    put_node(
        store,
        BTreeMap::from([("wide".to_owned(), DirectoryEntry::Directory(wide))]),
    )
    .await
}

#[tokio::test]
async fn a_wide_directory_whose_count_is_wrong_is_refused() {
    let store = store();
    let (_, wide, _) = fixture(&store, 1_000, 0).await;
    let header = chunked_header(&store, wide).await;
    let root = root_with(
        &store,
        ChunkedTree::new(header.count + 1, header.root).unwrap(),
    )
    .await;
    assert!(matches!(
        MerkleTreeStore::new(&store)
            .walk_files(root, TreeWalkLimits::default())
            .await,
        Err(MerkleTreeError::MalformedDirectory(
            "the entry count does not match the entries"
        ))
    ));
}

#[tokio::test]
async fn a_subtree_under_another_edge_is_refused() {
    let store = store();
    let (_, wide, _) = fixture(&store, 1_000, 0).await;
    let header = chunked_header(&store, wide).await;
    // Each node is well formed on its own, but two of the root's subtrees swap
    // places, so their entries sit under keys that are not their names'.
    let object = store.get(header.root.into()).await.unwrap();
    let node = decode_directory_chunk_node(&object.canonical_bytes).unwrap();
    let mut children = node.children.clone();
    let (first, second) = (children[0].child, children[1].child);
    children[0] = DirectoryChunkChild {
        child: second,
        ..children[0]
    };
    children[1] = DirectoryChunkChild {
        child: first,
        ..children[1]
    };
    let swapped = DirectoryChunkNode::new(node.prefix, children, node.entries).unwrap();
    store
        .put_if_absent(swapped.id().into(), &swapped.canonical_bytes())
        .await
        .unwrap();
    let root = root_with(
        &store,
        ChunkedTree::new(header.count, swapped.id()).unwrap(),
    )
    .await;
    assert!(matches!(
        MerkleTreeStore::new(&store)
            .walk_files(root, TreeWalkLimits::default())
            .await,
        Err(MerkleTreeError::MalformedDirectory(
            "an entry is filed under another name's key"
        ))
    ));
}
