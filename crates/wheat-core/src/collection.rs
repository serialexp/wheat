use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, ApplicationMapId, CanonicalObjectError, ChangeMapId, ChangeObjectId,
    ChangeSetId, ConflictSetId, ContentMapId, ContentSetId, DirectoryChunkId, DirectoryEntry,
    ProjectedCommitSetId, RetainedSetId, TreeId, ViewLineageMapId,
};

macro_rules! child {
    ($name:ident, $id:ty) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            pub edge: u8,
            pub child: $id,
        }
    };
}

child!(ApplicationMapChild, ApplicationMapId);
child!(ChangeMapChild, ChangeMapId);
child!(ConflictSetChild, ConflictSetId);
child!(ContentMapChild, ContentMapId);
child!(ContentSetChild, ContentSetId);
child!(RetainedSetChild, RetainedSetId);
child!(ProjectedCommitSetChild, ProjectedCommitSetId);
child!(ChangeSetChild, ChangeSetId);
child!(ViewLineageMapChild, ViewLineageMapId);

/// One child of a [`DirectoryChunkNode`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryChunkChild {
    pub edge: u8,
    pub child: DirectoryChunkId,
}

/// An entry of a wide directory: its name and what the name holds. It sits in the
/// directory's radix at `directory_name_key(name)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryChunkEntry {
    pub name: String,
    pub entry: DirectoryEntry,
}

/// A node of a wide directory's radix over the BLAKE3 hash of each entry's name
/// ([`crate::directory_name_key`]). Every node branches at least twice: the root
/// holds more than [`crate::FLAT_DIRECTORY_MAX_ENTRIES`] entries and a single entry
/// below it is inlined in its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryChunkNode {
    pub prefix: Vec<u8>,
    pub children: Vec<DirectoryChunkChild>,
    /// Inline entries: the rest of each entry's key, with the entry.
    pub entries: Vec<(Vec<u8>, DirectoryChunkEntry)>,
}

impl DirectoryChunkNode {
    pub fn new(
        prefix: Vec<u8>,
        mut children: Vec<DirectoryChunkChild>,
        mut entries: Vec<(Vec<u8>, DirectoryChunkEntry)>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            children,
            entries,
        };
        crate::wire::validate_directory_chunk_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::directory_chunk_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> DirectoryChunkId {
        DirectoryChunkId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// A directory of more than [`crate::FLAT_DIRECTORY_MAX_ENTRIES`] entries: how many
/// it holds and the root of their radix. Its ID is the directory's [`TreeId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkedTree {
    pub count: u64,
    pub root: DirectoryChunkId,
}

impl ChunkedTree {
    pub fn new(count: u64, root: DirectoryChunkId) -> Result<Self, CanonicalObjectError> {
        let tree = Self { count, root };
        crate::wire::validate_chunked_tree(&tree)?;
        Ok(tree)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::chunked_tree_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> TreeId {
        TreeId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationMapNode {
    pub prefix: Vec<u8>,
    pub value: Option<ApplicationId>,
    pub children: Vec<ApplicationMapChild>,
    /// Inline entries: key suffixes with values inlined into this node.
    pub entries: Vec<(Vec<u8>, ApplicationId)>,
}

impl ApplicationMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<ApplicationId>,
        mut children: Vec<ApplicationMapChild>,
        mut entries: Vec<(Vec<u8>, ApplicationId)>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            value,
            children,
            entries,
        };
        crate::wire::validate_application_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::application_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ApplicationMapId {
        ApplicationMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewLineageMapNode {
    pub prefix: Vec<u8>,
    pub value: Option<RetainedSetId>,
    pub children: Vec<ViewLineageMapChild>,
    pub entries: Vec<(Vec<u8>, RetainedSetId)>,
}

impl ViewLineageMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<RetainedSetId>,
        mut children: Vec<ViewLineageMapChild>,
        mut entries: Vec<(Vec<u8>, RetainedSetId)>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            value,
            children,
            entries,
        };
        crate::wire::validate_view_lineage_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::view_lineage_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ViewLineageMapId {
        ViewLineageMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// A node of a view's content map: ContentId digest → the number of tree entries
/// naming that blob, at least 1.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentMapNode {
    pub prefix: Vec<u8>,
    pub value: Option<u32>,
    pub children: Vec<ContentMapChild>,
    pub entries: Vec<(Vec<u8>, u32)>,
}

impl ContentMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<u32>,
        mut children: Vec<ContentMapChild>,
        mut entries: Vec<(Vec<u8>, u32)>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            value,
            children,
            entries,
        };
        crate::wire::validate_content_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::content_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ContentMapId {
        ContentMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeMapNode {
    pub prefix: Vec<u8>,
    pub value: Option<ChangeObjectId>,
    pub children: Vec<ChangeMapChild>,
    pub entries: Vec<(Vec<u8>, ChangeObjectId)>,
}

impl ChangeMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<ChangeObjectId>,
        mut children: Vec<ChangeMapChild>,
        mut entries: Vec<(Vec<u8>, ChangeObjectId)>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            value,
            children,
            entries,
        };
        crate::wire::validate_change_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::change_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ChangeMapId {
        ChangeMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

macro_rules! set_node {
    ($node:ident, $child:ident, $id:ty, $validate:ident, $bytes:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct $node {
            pub prefix: Vec<u8>,
            pub present: bool,
            pub children: Vec<$child>,
            /// Inline entries: key suffixes for members inlined into this node.
            pub entries: Vec<Vec<u8>>,
        }

        impl $node {
            pub fn new(
                prefix: Vec<u8>,
                present: bool,
                mut children: Vec<$child>,
                mut entries: Vec<Vec<u8>>,
            ) -> Result<Self, CanonicalObjectError> {
                children.sort_by_key(|child| child.edge);
                entries.sort();
                let node = Self {
                    prefix,
                    present,
                    children,
                    entries,
                };
                crate::wire::$validate(&node)?;
                Ok(node)
            }

            #[must_use]
            pub fn canonical_bytes(&self) -> Vec<u8> {
                crate::wire::$bytes(self)
            }

            #[must_use]
            pub fn id(&self) -> $id {
                <$id>::of_canonical_bytes(&self.canonical_bytes())
            }
        }
    };
}

set_node!(
    ConflictSetNode,
    ConflictSetChild,
    ConflictSetId,
    validate_conflict_set_node,
    conflict_set_node_bytes
);
set_node!(
    ContentSetNode,
    ContentSetChild,
    ContentSetId,
    validate_content_set_node,
    content_set_node_bytes
);
set_node!(
    RetainedSetNode,
    RetainedSetChild,
    RetainedSetId,
    validate_retained_set_node,
    retained_set_node_bytes
);
set_node!(
    ProjectedCommitSetNode,
    ProjectedCommitSetChild,
    ProjectedCommitSetId,
    validate_projected_commit_set_node,
    projected_commit_set_node_bytes
);
set_node!(
    ChangeSetNode,
    ChangeSetChild,
    ChangeSetId,
    validate_change_set_node,
    change_set_node_bytes
);
