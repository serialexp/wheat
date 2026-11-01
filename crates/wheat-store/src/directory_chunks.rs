//! The radix of a wide directory: a [`wheat_core::ChunkedTree`]'s chunk nodes,
//! keyed by the BLAKE3 hash of each entry's name.

use wheat_core::{
    CanonicalObjectError, DirectoryChunkChild, DirectoryChunkEntry, DirectoryChunkId,
    DirectoryChunkNode, decode_directory_chunk_node,
};

use crate::{ObjectKind, radix};

/// The key of a directory entry: `directory_name_key(name)`.
pub(crate) type DirectoryNameKey = [u8; 32];

impl radix::Key for DirectoryNameKey {
    const LEN: usize = 32;
    fn bytes(&self) -> &[u8] {
        self
    }
    fn from_bytes(bytes: &[u8]) -> Self {
        bytes.try_into().expect("radix key length is fixed")
    }
}

pub(crate) struct DirectoryChunkCollection;
impl radix::Collection for DirectoryChunkCollection {
    type Id = DirectoryChunkId;
    type Key = DirectoryNameKey;
    type Value = DirectoryChunkEntry;
    type PublicNode = DirectoryChunkNode;
    const KIND: ObjectKind = ObjectKind::DirectoryChunkNode;
    const BUCKET: usize = wheat_core::DIRECTORY_CHUNK_BUCKET;
    fn decode(bytes: &[u8]) -> Result<Self::PublicNode, CanonicalObjectError> {
        decode_directory_chunk_node(bytes)
    }
    fn unpack(node: Self::PublicNode) -> radix::Node<Self::Id, Self::Value> {
        radix::Node {
            prefix: node.prefix,
            value: None,
            children: node
                .children
                .into_iter()
                .map(|c| (c.edge, c.child))
                .collect(),
            entries: node.entries,
        }
    }
    fn build(
        node: radix::Node<Self::Id, Self::Value>,
    ) -> Result<Self::PublicNode, CanonicalObjectError> {
        // A terminal node holds a single entry, and a chunked tree holds more than
        // `FLAT_DIRECTORY_MAX_ENTRIES`: its radix never has one.
        if node.value.is_some() {
            return Err(CanonicalObjectError::InvalidCollection(
                "a directory chunk node holds no terminal value",
            ));
        }
        DirectoryChunkNode::new(
            node.prefix,
            node.children
                .into_iter()
                .map(|(edge, child)| DirectoryChunkChild { edge, child })
                .collect(),
            node.entries,
        )
    }
    fn id(node: &Self::PublicNode) -> Self::Id {
        node.id()
    }
    fn canonical_bytes(node: &Self::PublicNode) -> Vec<u8> {
        node.canonical_bytes()
    }
}

#[cfg(test)]
mod tests {
    //! Directory chunks are the one collection whose buckets hold two entries, so
    //! these tests cover the radix's bucket handling: every incremental update must
    //! land on the node a fresh build makes, and every diff must list exactly the
    //! entries that differ, in key order.

    use std::{collections::BTreeMap, sync::Arc};

    use object_store::memory::InMemory;
    use wheat_core::{
        CanonicalObject, ContentId, DirectoryEntry, FileKind, FileValue, TreeId, build_directory,
        decode_chunked_tree, directory_name_key,
    };

    use super::*;
    use crate::{
        ImmutableObjectStore, ObjectStoreAdapter,
        radix::{DiffEntry, DiffLimits, Store},
    };

    type Entries = BTreeMap<DirectoryNameKey, DirectoryChunkEntry>;

    fn entry(name: &str, version: u8) -> (DirectoryNameKey, DirectoryChunkEntry) {
        let entry = DirectoryEntry::File(FileValue {
            content: ContentId::of(&[name.as_bytes(), &[version]].concat()),
            kind: FileKind::Regular,
        });
        (
            directory_name_key(name),
            DirectoryChunkEntry {
                name: name.to_owned(),
                entry,
            },
        )
    }

    fn named(names: impl IntoIterator<Item = usize>) -> Entries {
        names
            .into_iter()
            .map(|index| entry(&format!("n{index}"), 0))
            .collect()
    }

    /// Builds `entries` from scratch, stores every object and returns the root chunk.
    async fn fresh<S: ImmutableObjectStore>(objects: &S, entries: &Entries) -> DirectoryChunkId {
        let mut written = Vec::new();
        let tree: TreeId = build_directory(
            entries
                .values()
                .map(|entry| (entry.name.clone(), entry.entry))
                .collect(),
            |object: CanonicalObject| written.push((object.id, object.bytes)),
        )
        .unwrap();
        let root_bytes = written
            .iter()
            .find(|(id, _)| *id == tree.into())
            .map(|(_, bytes)| bytes.clone())
            .expect("the tree is written");
        for (id, bytes) in &written {
            objects.put_if_absent(*id, bytes).await.unwrap();
        }
        decode_chunked_tree(&root_bytes).unwrap().root
    }

    fn expected_diff(
        left: &Entries,
        right: &Entries,
    ) -> Vec<DiffEntry<DirectoryNameKey, DirectoryChunkEntry>> {
        let mut keys: Vec<_> = left.keys().chain(right.keys()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter()
            .filter_map(|key| match (left.get(&key), right.get(&key)) {
                (Some(old), Some(new)) if old != new => {
                    Some(DiffEntry::Changed(key, old.clone(), new.clone()))
                }
                (Some(old), None) => Some(DiffEntry::Removed(key, old.clone())),
                (None, Some(new)) => Some(DiffEntry::Added(key, new.clone())),
                _ => None,
            })
            .collect()
    }

    /// A deterministic stream of edit batches over the names `n0..n{span}`.
    struct Edits {
        state: u64,
        span: usize,
    }

    impl Edits {
        fn next(&mut self) -> usize {
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.state >> 33) as usize
        }

        /// A batch of up to `size` edits that keeps the directory chunked.
        fn batch(
            &mut self,
            current: &Entries,
            size: usize,
            version: u8,
        ) -> Vec<(DirectoryNameKey, Option<DirectoryChunkEntry>)> {
            let mut batch = BTreeMap::new();
            let mut count = current.len();
            for _ in 0..size {
                let (key, value) = entry(&format!("n{}", self.next() % self.span), version);
                let present =
                    current.contains_key(&key) || batch.get(&key).is_some_and(Option::is_some);
                let edit = match self.next() % 3 {
                    0 if present && count > 300 => {
                        count -= 1;
                        None
                    }
                    _ => {
                        if !present {
                            count += 1;
                        }
                        Some(value)
                    }
                };
                batch.insert(key, edit);
            }
            batch.into_iter().collect()
        }
    }

    fn apply(
        current: &Entries,
        batch: &[(DirectoryNameKey, Option<DirectoryChunkEntry>)],
    ) -> Entries {
        let mut next = current.clone();
        for (key, edit) in batch {
            match edit {
                Some(value) => next.insert(*key, value.clone()),
                None => next.remove(key),
            };
        }
        next
    }

    #[tokio::test]
    async fn incremental_updates_land_on_the_fresh_build() {
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "directory-chunks");
        let store = Store::<_, DirectoryChunkCollection>::new(&objects);
        for (initial, span, seed) in [(300, 400, 1), (3_000, 3_400, 2)] {
            let mut edits = Edits { state: seed, span };
            let mut current = named(0..initial);
            let mut root = fresh(&objects, &current).await;
            for (round, size) in [1, 1, 2, 3, 5, 8, 40, 200, 1, 2].into_iter().enumerate() {
                let batch = edits.batch(&current, size, round as u8 + 1);
                let next = apply(&current, &batch);
                let updated = store.update_batch(root, batch).await.unwrap();
                assert_eq!(updated, fresh(&objects, &next).await, "round {round}");
                assert_eq!(
                    store
                        .paired_diff(root, updated, DiffLimits::any_size())
                        .await
                        .unwrap(),
                    expected_diff(&current, &next),
                    "round {round}"
                );
                assert_eq!(
                    store
                        .paired_diff(updated, root, DiffLimits::any_size())
                        .await
                        .unwrap(),
                    expected_diff(&next, &current),
                    "round {round}, reversed"
                );
                current = next;
                root = updated;
            }
            for (key, value) in &current {
                assert_eq!(store.get(root, *key).await.unwrap().as_ref(), Some(value));
            }
            let absent = entry("absent", 0).0;
            assert_eq!(store.get(root, absent).await.unwrap(), None);
            let mut cursor = store.cursor(root).await.unwrap();
            let mut walked = Vec::new();
            while let Some(item) = cursor.next().await.unwrap() {
                walked.push(item);
            }
            assert_eq!(walked, current.into_iter().collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn diffs_pair_buckets_with_children_on_the_same_byte() {
        // Two directories that agree on most names but group a key byte as a bucket
        // on one side and as a child on the other.
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "directory-buckets");
        let store = Store::<_, DirectoryChunkCollection>::new(&objects);
        let base = named(0..400);
        let mut by_first: BTreeMap<u8, Vec<usize>> = BTreeMap::new();
        for index in 400..20_000 {
            by_first
                .entry(directory_name_key(&format!("n{index}"))[0])
                .or_default()
                .push(index);
        }
        let bucketed = base
            .keys()
            .map(|key| key[0])
            .collect::<std::collections::BTreeSet<_>>();
        // Bytes where `base` holds a bucket or nothing; adding three names there
        // turns it into a child.
        let grown = apply(
            &base,
            &by_first
                .iter()
                .filter(|(byte, _)| {
                    !bucketed.contains(byte)
                        || base.keys().filter(|key| key[0] == **byte).count() <= 2
                })
                .take(20)
                .flat_map(|(_, names)| names.iter().take(3))
                .map(|index| {
                    let (key, value) = entry(&format!("n{index}"), 0);
                    (key, Some(value))
                })
                .collect::<Vec<_>>(),
        );
        let left = fresh(&objects, &base).await;
        let right = fresh(&objects, &grown).await;
        assert_eq!(
            store
                .paired_diff(left, right, DiffLimits::any_size())
                .await
                .unwrap(),
            expected_diff(&base, &grown)
        );
        assert_eq!(
            store
                .paired_diff(right, left, DiffLimits::any_size())
                .await
                .unwrap(),
            expected_diff(&grown, &base)
        );
        assert_eq!(store.union(left, right).await.unwrap(), right);
    }
}
