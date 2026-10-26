//! The one way the store reads a directory, whichever its layout: a flat `TreeV1`
//! record, or a chunked tree whose entries sit in a radix keyed by the BLAKE3 hash
//! of their names (`docs/design/wheat-wide-directories.md`).
//!
//! A name lookup reads the chunks on its key's path. Listing a directory in name
//! order reads a chunked one whole and sorts it. Two chunked directories are
//! compared through their radix, reading only the chunks where they differ.

use std::collections::{BTreeMap, HashMap, hash_map};

use wheat_core::{
    ChunkedTree, DirectoryChunkEntry, DirectoryChunkId, DirectoryEntry, DirectoryNode, TreeId,
    decode_chunked_tree, decode_directory_node, directory_name_key,
};

use super::{MerkleTreeError, MerkleTreeStore};
use crate::{
    ImmutableObjectStore, ObjectKind, ObjectStoreError,
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

/// A directory opened from its [`TreeId`].
pub(crate) enum Directory {
    /// A `TreeV1` record, decoded whole.
    Flat(DirectoryNode),
    /// A chunked tree's header, with the chunks lookups have read so far.
    Chunked {
        tree: ChunkedTree,
        chunks: HashMap<DirectoryChunkId, ChunkNode>,
    },
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

/// The entries two directories hold, in name order, for pairing by name. Entries
/// both hold alike may be left out.
pub(crate) struct DirectoryDiff {
    pub old: BTreeMap<String, DirectoryEntry>,
    pub new: BTreeMap<String, DirectoryEntry>,
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
    ) -> Result<ChunkNode, MerkleTreeError> {
        count.node(limits)?;
        let (node, size) = radix::Store::<S, DirectoryChunkCollection>::new(self.objects)
            .read_at_with_size(id, position)
            .await
            .map_err(malformed)?;
        ReadCount::bytes(limits, size)?;
        count.entries(limits, node.entries.len())?;
        Ok(node)
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
        let key = directory_name_key(name);
        let mut id = tree.root;
        let mut position = 0;
        loop {
            let node = match chunks.entry(id) {
                hash_map::Entry::Occupied(entry) => entry.into_mut(),
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(self.read_chunk(id, position, limits, count).await?)
                }
            };
            let end = position + node.prefix.len();
            if key[position..end] != node.prefix[..] {
                return Ok(None);
            }
            let rest = &key[end..];
            if let Ok(index) = node
                .entries
                .binary_search_by(|(suffix, _)| suffix.as_slice().cmp(rest))
            {
                // Two names cannot share a key, so another name here means `name`
                // is absent.
                let found = &node.entries[index].1;
                return Ok((found.name == name).then_some(found.entry));
            }
            let Some(edge) = rest.first() else {
                return Ok(None);
            };
            let Ok(index) = node.children.binary_search_by_key(edge, |child| child.0) else {
                return Ok(None);
            };
            id = node.children[index].1;
            position = end + 1;
        }
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
        let tree = match directory {
            Directory::Flat(node) => return Ok(node.entries),
            Directory::Chunked { tree, .. } => tree,
        };
        let mut entries = BTreeMap::new();
        // The key bytes above the node being read. Nodes pop depth first, so a
        // node's ancestors' bytes are still in place when it is reached.
        let mut key = Vec::with_capacity(32);
        let mut stack = vec![(tree.root, 0, None)];
        while let Some((id, above, edge)) = stack.pop() {
            key.truncate(above);
            key.extend(edge);
            let node = self.read_chunk(id, key.len(), limits, count).await?;
            key.extend_from_slice(&node.prefix);
            let end = key.len();
            for (_, entry) in node.entries {
                // The node checked that the suffix ends the name's key; this checks
                // the bytes above it.
                if directory_name_key(&entry.name)[..end] != key[..] {
                    return Err(MerkleTreeError::MalformedDirectory(
                        "an entry is filed under another name's key",
                    ));
                }
                entries.insert(entry.name, entry.entry);
            }
            for (edge, child) in node.children.into_iter().rev() {
                stack.push((child, end, Some(edge)));
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
