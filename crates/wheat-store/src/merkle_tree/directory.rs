//! The one way the store reads a directory, whichever its layout: a flat `TreeV1`
//! record, or a chunked tree whose entries sit in a radix keyed by the BLAKE3 hash
//! of their names (`docs/design/wheat-wide-directories.md`).
//!
//! A name lookup reads the chunks on its key's path. Listing a directory in name
//! order reads a chunked one whole and sorts it. Two chunked directories are
//! compared through their radix, reading only the chunks where they differ.
//!
//! Writing a change to a chunked directory rewrites the chunks on the changed
//! names' paths and its header; a directory that crosses the flat threshold is
//! rebuilt in the other layout.

use std::collections::{BTreeMap, HashMap, hash_map};

use async_trait::async_trait;
use wheat_core::{
    ChunkedTree, DirectoryChunkEntry, DirectoryChunkId, DirectoryEntry, DirectoryNode,
    FLAT_DIRECTORY_MAX_ENTRIES, ObjectId, TreeId, build_directory, decode_chunked_tree,
    decode_directory_node, directory_name_key,
};

use super::{MerkleTreeError, MerkleTreeStore};
use crate::{
    ImmutableObjectStore, ObjectKind, ObjectStoreError, StoredObject,
    directory_chunks::DirectoryChunkCollection,
    radix::{self, DiffEntry, DiffError, DiffLimits, Node},
};

/// Bounds on what reading directories may cost: the objects read, the entries
/// they decode, and the canonical bytes of any one object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadLimits {
    pub max_nodes: usize,
    pub max_entries: usize,
    pub max_node_bytes: usize,
}

impl ReadLimits {
    pub(crate) const UNBOUNDED: Self = Self {
        max_nodes: usize::MAX,
        max_entries: usize::MAX,
        max_node_bytes: usize::MAX,
    };
}

/// What reading directories has cost so far, against [`ReadLimits`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReadCount {
    pub nodes: usize,
    pub entries: usize,
}

impl ReadCount {
    /// Charges one object before it is read.
    fn node(&mut self, limits: ReadLimits) -> Result<(), MerkleTreeError> {
        if self.nodes >= limits.max_nodes {
            return Err(MerkleTreeError::NodeLimitExceeded {
                limit: limits.max_nodes,
            });
        }
        self.nodes += 1;
        Ok(())
    }

    fn bytes(limits: ReadLimits, size: usize) -> Result<(), MerkleTreeError> {
        if size > limits.max_node_bytes {
            return Err(MerkleTreeError::NodeBytesExceeded {
                size,
                limit: limits.max_node_bytes,
            });
        }
        Ok(())
    }

    fn entries(&mut self, limits: ReadLimits, count: usize) -> Result<(), MerkleTreeError> {
        let entries = self.entries.saturating_add(count);
        if entries > limits.max_entries {
            return Err(MerkleTreeError::EntryLimitExceeded {
                limit: limits.max_entries,
            });
        }
        self.entries = entries;
        Ok(())
    }
}

type ChunkNode = Node<DirectoryChunkId, DirectoryChunkEntry>;

/// A chunk read from the store: decoded, and its canonical bytes for a write to
/// read again.
pub(crate) struct Chunk {
    node: ChunkNode,
    bytes: bytes::Bytes,
}

/// A directory opened from its [`TreeId`].
pub(crate) enum Directory {
    /// A `TreeV1` record, decoded whole.
    Flat(DirectoryNode),
    /// A chunked tree's header, with the chunks lookups have read so far.
    Chunked {
        tree: ChunkedTree,
        chunks: HashMap<DirectoryChunkId, Chunk>,
    },
}

/// What a chunked directory holds at a name's key.
enum Slot {
    Empty,
    Holds(DirectoryEntry),
    /// Another name, whose key is the same.
    Collides,
}

/// The store a chunked directory's update goes through: the radix update reads the
/// chunks on the changed names' paths, which looking the names up has just read,
/// so those come from memory.
struct Remembered<'d, S> {
    objects: &'d S,
    chunks: &'d HashMap<DirectoryChunkId, Chunk>,
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for Remembered<'_, S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.objects.put_if_absent(id, canonical_bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        match self
            .chunks
            .get(&DirectoryChunkId::from_digest(*id.as_bytes()))
        {
            Some(chunk) => Ok(StoredObject {
                id,
                kind: ObjectKind::DirectoryChunkNode,
                canonical_bytes: chunk.bytes.clone(),
            }),
            None => self.objects.get(id).await,
        }
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.objects.exists_many(ids).await
    }
}

impl Directory {
    /// The entries opening it decoded: all of a flat directory, none of a chunked one.
    pub(crate) fn decoded_entries(&self) -> usize {
        match self {
            Directory::Flat(node) => node.entries.len(),
            Directory::Chunked { .. } => 0,
        }
    }
}

/// A directory as it is stored: a flat record's entries, or a chunked tree's header,
/// whose chunks a [`ChunkWalk`] reads.
pub enum StoredDirectory {
    Flat(DirectoryNode),
    Chunked(ChunkedTree),
}

/// A depth-first walk over a chunked directory's chunks that its caller steers, so a
/// walk that proves something about each chunk can skip the ones it proved before.
/// [`Self::next_chunk`] names the next chunk and the key bytes above it. The caller then
/// either [`Self::read`]s it, which checks that each of its entries is filed under
/// its name's key and queues the chunks below it, or asks for the next, skipping
/// the chunk and everything below it.
pub struct ChunkWalk {
    /// Chunks to visit, with the length of the key above the edge to each and the
    /// edge's byte.
    stack: Vec<(DirectoryChunkId, usize, Option<u8>)>,
    /// The key bytes above the chunk `next_chunk` returned.
    key: Vec<u8>,
    current: Option<DirectoryChunkId>,
}

/// What a [`ChunkWalk`] read in one chunk.
pub struct WalkedChunk {
    pub entries: Vec<DirectoryChunkEntry>,
    pub children: Vec<DirectoryChunkId>,
}

impl ChunkWalk {
    pub fn new(tree: &ChunkedTree) -> Self {
        Self {
            stack: vec![(tree.root, 0, None)],
            key: Vec::with_capacity(32),
            current: None,
        }
    }

    /// The next chunk, and the key bytes above it, or `None` when the walk is done.
    pub fn next_chunk(&mut self) -> Option<(DirectoryChunkId, &[u8])> {
        let (id, above, edge) = self.stack.pop()?;
        // Chunks pop depth first, so a chunk's ancestors' bytes are still in place.
        self.key.truncate(above);
        self.key.extend(edge);
        self.current = Some(id);
        Some((id, &self.key))
    }

    /// Reads the chunk [`Self::next_chunk`] returned.
    ///
    /// # Panics
    ///
    /// When `next_chunk` has not returned a chunk since the last read.
    pub async fn read<S: ImmutableObjectStore>(
        &mut self,
        trees: &MerkleTreeStore<'_, S>,
    ) -> Result<WalkedChunk, MerkleTreeError> {
        let id = self.current.take().expect("a read follows next_chunk");
        let chunk = trees
            .read_chunk(
                id,
                self.key.len(),
                ReadLimits::UNBOUNDED,
                &mut ReadCount::default(),
            )
            .await?;
        self.visit(chunk.node)
    }

    /// Checks `node`, the chunk [`Self::next_chunk`] returned, and queues its children.
    fn visit(&mut self, node: ChunkNode) -> Result<WalkedChunk, MerkleTreeError> {
        self.current = None;
        self.key.extend_from_slice(&node.prefix);
        let end = self.key.len();
        let mut entries = Vec::with_capacity(node.entries.len());
        for (_, entry) in node.entries {
            // The node checked that the suffix ends the name's key; this checks the
            // bytes above it.
            if directory_name_key(&entry.name)[..end] != self.key[..] {
                return Err(MerkleTreeError::MalformedDirectory(
                    "an entry is filed under another name's key",
                ));
            }
            entries.push(entry);
        }
        let mut children = Vec::with_capacity(node.children.len());
        for (edge, child) in node.children.into_iter().rev() {
            self.stack.push((child, end, Some(edge)));
            children.push(child);
        }
        Ok(WalkedChunk { entries, children })
    }
}

/// The entries two directories hold, in name order, for pairing by name. Entries
/// both hold alike may be left out.
pub(crate) struct DirectoryDiff {
    pub old: BTreeMap<String, DirectoryEntry>,
    pub new: BTreeMap<String, DirectoryEntry>,
}

fn apply(
    entries: &mut BTreeMap<String, DirectoryEntry>,
    changes: BTreeMap<String, Option<DirectoryEntry>>,
) {
    for (name, change) in changes {
        match change {
            Some(entry) => entries.insert(name, entry),
            None => entries.remove(&name),
        };
    }
}

fn malformed(error: radix::Error) -> MerkleTreeError {
    match error {
        radix::Error::Store(error) => MerkleTreeError::Store(error),
        radix::Error::Decode(error) => MerkleTreeError::Decode(error),
        radix::Error::InvalidRootDepth
        | radix::Error::InvalidChildDepth
        | radix::Error::InvalidTerminalNode => {
            MerkleTreeError::MalformedDirectory("a chunk node is out of shape for its position")
        }
    }
}

impl<S: ImmutableObjectStore> MerkleTreeStore<'_, S> {
    /// Opens the directory `id`: decodes a flat one whole, or reads a chunked one's
    /// header.
    pub(crate) async fn open_directory(
        &self,
        id: TreeId,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<Directory, MerkleTreeError> {
        count.node(limits)?;
        let object = self.objects.get(id.into()).await?;
        ReadCount::bytes(limits, object.canonical_bytes.len())?;
        match object.kind {
            ObjectKind::Tree => {
                let node = decode_directory_node(&object.canonical_bytes)?;
                count.entries(limits, node.entries.len())?;
                Ok(Directory::Flat(node))
            }
            ObjectKind::ChunkedTree => Ok(Directory::Chunked {
                tree: decode_chunked_tree(&object.canonical_bytes)?,
                chunks: HashMap::new(),
            }),
            actual => Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::Tree,
                actual,
            }
            .into()),
        }
    }

    /// Reads directory `id` as it is stored: a chunked one's chunks are left for a
    /// [`ChunkWalk`].
    pub async fn read_stored_directory(
        &self,
        id: TreeId,
    ) -> Result<StoredDirectory, MerkleTreeError> {
        let directory = self
            .open_directory(id, ReadLimits::UNBOUNDED, &mut ReadCount::default())
            .await?;
        Ok(match directory {
            Directory::Flat(node) => StoredDirectory::Flat(node),
            Directory::Chunked { tree, .. } => StoredDirectory::Chunked(tree),
        })
    }

    /// Reads directory `id` whole, in name order.
    pub(crate) async fn read_directory(
        &self,
        id: TreeId,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<DirectoryNode, MerkleTreeError> {
        let directory = self.open_directory(id, limits, count).await?;
        Ok(DirectoryNode {
            entries: self.directory_entries(directory, limits, count).await?,
        })
    }

    async fn read_chunk(
        &self,
        id: DirectoryChunkId,
        position: usize,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<Chunk, MerkleTreeError> {
        count.node(limits)?;
        let object = self.objects.get(id.into()).await?;
        ReadCount::bytes(limits, object.canonical_bytes.len())?;
        let node = radix::Store::<S, DirectoryChunkCollection>::new(self.objects)
            .decode_at(&object, position)
            .map_err(malformed)?;
        count.entries(limits, node.entries.len())?;
        Ok(Chunk {
            node,
            bytes: object.canonical_bytes,
        })
    }

    /// What `directory` holds under `name`. A chunked directory reads the chunks on
    /// the name's key path that earlier lookups have not, so a batch of lookups
    /// reads each chunk at most once.
    pub(crate) async fn directory_get(
        &self,
        directory: &mut Directory,
        name: &str,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<Option<DirectoryEntry>, MerkleTreeError> {
        let (tree, chunks) = match directory {
            Directory::Flat(node) => return Ok(node.entries.get(name).copied()),
            Directory::Chunked { tree, chunks } => (tree, chunks),
        };
        Ok(
            match self.slot(tree.root, chunks, name, limits, count).await? {
                Slot::Holds(entry) => Some(entry),
                // Two names cannot share a key, so another name there means `name` is
                // absent.
                Slot::Empty | Slot::Collides => None,
            },
        )
    }

    async fn slot(
        &self,
        root: DirectoryChunkId,
        chunks: &mut HashMap<DirectoryChunkId, Chunk>,
        name: &str,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<Slot, MerkleTreeError> {
        let key = directory_name_key(name);
        let mut id = root;
        let mut position = 0;
        loop {
            let node = match chunks.entry(id) {
                hash_map::Entry::Occupied(entry) => &entry.into_mut().node,
                hash_map::Entry::Vacant(entry) => {
                    &entry
                        .insert(self.read_chunk(id, position, limits, count).await?)
                        .node
                }
            };
            let end = position + node.prefix.len();
            if key[position..end] != node.prefix[..] {
                return Ok(Slot::Empty);
            }
            let rest = &key[end..];
            if let Ok(index) = node
                .entries
                .binary_search_by(|(suffix, _)| suffix.as_slice().cmp(rest))
            {
                let found = &node.entries[index].1;
                return Ok(if found.name == name {
                    Slot::Holds(found.entry)
                } else {
                    Slot::Collides
                });
            }
            let Some(edge) = rest.first() else {
                return Ok(Slot::Empty);
            };
            let Ok(index) = node.children.binary_search_by_key(edge, |child| child.0) else {
                return Ok(Slot::Empty);
            };
            id = node.children[index].1;
            position = end + 1;
        }
    }

    /// Writes `directory` with `changes` applied (`None` removes a name) and returns
    /// its ID, or `None` when nothing is left in it.
    ///
    /// A chunked directory rewrites only the chunks on the changed names' paths,
    /// reading those it has not read already. A directory that crosses
    /// [`FLAT_DIRECTORY_MAX_ENTRIES`] either way is rebuilt in the other layout,
    /// reading a chunked one whole.
    pub(crate) async fn write_directory(
        &self,
        directory: Directory,
        changes: BTreeMap<String, Option<DirectoryEntry>>,
    ) -> Result<Option<TreeId>, MerkleTreeError> {
        let (limits, mut count) = (ReadLimits::UNBOUNDED, ReadCount::default());
        let (tree, mut chunks) = match directory {
            Directory::Flat(mut node) => {
                apply(&mut node.entries, changes);
                return self.write_entries(node.entries).await;
            }
            Directory::Chunked { tree, chunks } => (tree, chunks),
        };
        let mut size = tree.count;
        let mut edits = Vec::with_capacity(changes.len());
        for (name, change) in &changes {
            match (
                self.slot(tree.root, &mut chunks, name, limits, &mut count)
                    .await?,
                change,
            ) {
                (Slot::Collides, Some(_)) => {
                    return Err(MerkleTreeError::MalformedDirectory(
                        "two names in the directory have the same key",
                    ));
                }
                // Removing an absent name changes nothing; the key is the other
                // name's.
                (Slot::Collides, None) => continue,
                (Slot::Empty, Some(_)) => size += 1,
                (Slot::Holds(_), None) => size -= 1,
                _ => {}
            }
            edits.push((
                directory_name_key(name),
                change.map(|entry| DirectoryChunkEntry {
                    name: name.clone(),
                    entry,
                }),
            ));
        }
        if size <= FLAT_DIRECTORY_MAX_ENTRIES as u64 {
            let mut entries = self
                .directory_entries(Directory::Chunked { tree, chunks }, limits, &mut count)
                .await?;
            apply(&mut entries, changes);
            return self.write_entries(entries).await;
        }
        let objects = Remembered {
            objects: self.objects,
            chunks: &chunks,
        };
        let root = radix::Store::<_, DirectoryChunkCollection>::new(&objects)
            .update_batch(tree.root, edits)
            .await
            .map_err(malformed)?;
        let header = ChunkedTree::new(size, root)?;
        let id = header.id();
        self.objects
            .put_if_absent(id.into(), &header.canonical_bytes())
            .await?;
        Ok(Some(id))
    }

    /// Writes a directory of `entries` from scratch, in the layout its size calls
    /// for, and returns its ID; `None` when it is empty.
    pub(crate) async fn write_entries(
        &self,
        entries: BTreeMap<String, DirectoryEntry>,
    ) -> Result<Option<TreeId>, MerkleTreeError> {
        if entries.is_empty() {
            return Ok(None);
        }
        let mut objects = Vec::new();
        let id = build_directory(entries, |object| objects.push(object))?;
        for object in objects {
            self.objects.put_if_absent(object.id, &object.bytes).await?;
        }
        Ok(Some(id))
    }

    /// Every entry of `directory`, in name order. A chunked directory is read whole,
    /// checking that each entry sits at its name's key and that the header's count
    /// is right, then sorted.
    pub(crate) async fn directory_entries(
        &self,
        directory: Directory,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<BTreeMap<String, DirectoryEntry>, MerkleTreeError> {
        let (tree, mut chunks) = match directory {
            Directory::Flat(node) => return Ok(node.entries),
            Directory::Chunked { tree, chunks } => (tree, chunks),
        };
        let mut entries = BTreeMap::new();
        let mut walk = ChunkWalk::new(&tree);
        while let Some((id, key)) = walk.next_chunk() {
            // Lookups read chunks at the same positions.
            let node = match chunks.remove(&id) {
                Some(chunk) => chunk.node,
                None => self.read_chunk(id, key.len(), limits, count).await?.node,
            };
            for entry in walk.visit(node)?.entries {
                entries.insert(entry.name, entry.entry);
            }
        }
        if entries.len() as u64 != tree.count {
            return Err(MerkleTreeError::MalformedDirectory(
                "the entry count does not match the entries",
            ));
        }
        Ok(entries)
    }

    /// The entries of `old` and `new` (`None` is an empty directory) to pair by name.
    /// Two chunked directories are compared through their radix: only the chunks
    /// where they differ are read and only differing entries are returned. Otherwise
    /// both are read whole.
    ///
    /// Chunk nodes the radix diff reads are not held to `max_node_bytes`: the format
    /// bounds them (at most 512 entries of names up to 255 bytes).
    pub(crate) async fn directory_diff(
        &self,
        old: Option<TreeId>,
        new: Option<TreeId>,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<DirectoryDiff, MerkleTreeError> {
        let old = match old {
            Some(id) => Some(self.open_directory(id, limits, count).await?),
            None => None,
        };
        let new = match new {
            Some(id) => Some(self.open_directory(id, limits, count).await?),
            None => None,
        };
        if let (
            Some(Directory::Chunked { tree: old, .. }),
            Some(Directory::Chunked { tree: new, .. }),
        ) = (&old, &new)
        {
            return self.chunked_diff(old.root, new.root, limits, count).await;
        }
        let old = match old {
            Some(directory) => self.directory_entries(directory, limits, count).await?,
            None => BTreeMap::new(),
        };
        let new = match new {
            Some(directory) => self.directory_entries(directory, limits, count).await?,
            None => BTreeMap::new(),
        };
        Ok(DirectoryDiff { old, new })
    }

    async fn chunked_diff(
        &self,
        old: DirectoryChunkId,
        new: DirectoryChunkId,
        limits: ReadLimits,
        count: &mut ReadCount,
    ) -> Result<DirectoryDiff, MerkleTreeError> {
        let diff_limits = DiffLimits {
            max_changes: limits.max_entries.saturating_sub(count.entries),
            max_nodes: limits.max_nodes.saturating_sub(count.nodes),
            // Pending work holds nodes and entries the two limits above bound.
            max_pending: usize::MAX,
        };
        let (changes, nodes) = radix::Store::<S, DirectoryChunkCollection>::new(self.objects)
            .paired_diff_counted(old, new, diff_limits)
            .await
            .map_err(|error| match error {
                DiffError::Radix(error) => malformed(error),
                DiffError::ChangeLimitExceeded { .. } => MerkleTreeError::EntryLimitExceeded {
                    limit: limits.max_entries,
                },
                DiffError::NodeLimitExceeded { .. } | DiffError::PendingLimitExceeded { .. } => {
                    MerkleTreeError::NodeLimitExceeded {
                        limit: limits.max_nodes,
                    }
                }
            })?;
        count.nodes += nodes;
        let mut diff = DirectoryDiff {
            old: BTreeMap::new(),
            new: BTreeMap::new(),
        };
        for change in changes {
            let (old, new) = match change {
                DiffEntry::Added(_, new) => (None, Some(new)),
                DiffEntry::Removed(_, old) => (Some(old), None),
                DiffEntry::Changed(_, old, new) => (Some(old), Some(new)),
            };
            if let Some(old) = old {
                diff.old.insert(old.name, old.entry);
            }
            if let Some(new) = new {
                diff.new.insert(new.name, new.entry);
            }
        }
        count.entries(limits, diff.old.len() + diff.new.len())?;
        Ok(diff)
    }
}
