use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_binschema::generated as wire;

use crate::{
    Actor, Application, ApplicationMapChild, ApplicationMapNode, ApplicationResult, Change,
    ChangeId, ChangeMetadata, ChangeRevision, Conflict, ConflictSetChild, ConflictSetNode,
    ContentId, ContentManifestId, ContentSetChild, ContentSetNode, DirectoryEntry, DirectoryNode,
    FileEdit, PathConflict, RepositoryState, RepositoryView, RetainedSetChild, RetainedSetNode,
    RevisionId, RevisionMapChild, RevisionMapNode, RevisionMetadata, SourceProvenance, Timestamp,
    Transformation, Tree, TreeId,
};

const BLAKE3_256: u8 = 1;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CanonicalObjectError {
    #[error("Wheat object does not decode: {0}")]
    Decode(String),
    #[error("Wheat object is not canonically encoded")]
    NonCanonicalEncoding,
    #[error("Wheat object violates a canonical collection invariant: {0}")]
    InvalidCollection(&'static str),
    #[error("Wheat content ID uses unsupported algorithm {0}")]
    UnsupportedContentAlgorithm(u8),
    #[error("Wheat object has kind {actual:?}, expected {expected:?}")]
    WrongKind {
        expected: CanonicalObjectKind,
        actual: CanonicalObjectKind,
    },
    #[error("invalid directory component {0:?}")]
    InvalidDirectoryComponent(String),
    #[error("invalid branch name {0:?}")]
    InvalidBranchName(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalObjectKind {
    Tree,
    Change,
    Revision,
    Application,
    Conflict,
    View,
    ContentManifest,
    RepositoryState,
    BranchManifest,
    RepositoryEnvelope,
    RevisionMapNode,
    ApplicationMapNode,
    ConflictSetNode,
    ContentSetNode,
    RetainedSetNode,
}

pub fn validate_canonical_object(
    bytes: &[u8],
) -> Result<CanonicalObjectKind, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    Ok(object_kind(&object))
}

fn decode_validated(bytes: &[u8]) -> Result<wire::WheatObject, CanonicalObjectError> {
    let object = wire::WheatObject::decode(bytes)
        .map_err(|error| CanonicalObjectError::Decode(format!("{error:?}")))?;
    validate_object(&object)?;
    let encoded = object
        .encode()
        .map_err(|error| CanonicalObjectError::Decode(format!("{error:?}")))?;
    if encoded != bytes {
        return Err(CanonicalObjectError::NonCanonicalEncoding);
    }
    Ok(object)
}

pub fn decode_view_record(bytes: &[u8]) -> Result<crate::ViewRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ViewV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::View,
            actual: object_kind(&object),
        });
    };
    Ok(crate::ViewRecord {
        tree: TreeId::from_digest(digest_domain(&v.tree.digest)?),
        revisions: crate::RevisionMapId::from_digest(digest_domain(&v.revisions.digest)?),
        applications: crate::ApplicationMapId::from_digest(digest_domain(&v.applications.digest)?),
        unresolved_conflicts: crate::ConflictSetId::from_digest(digest_domain(
            &v.unresolved_conflicts.digest,
        )?),
        required_content: ContentManifestId::from_digest(digest_domain(
            &v.required_content.digest,
        )?),
    })
}

pub fn decode_content_manifest_record(
    bytes: &[u8],
) -> Result<crate::ContentManifestRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ContentManifestV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ContentManifest,
            actual: object_kind(&object),
        });
    };
    Ok(crate::ContentManifestRecord {
        content: crate::ContentSetId::from_digest(digest_domain(&v.content.digest)?),
    })
}

pub fn decode_repository_state_record(
    bytes: &[u8],
) -> Result<crate::RepositoryStateRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::RepositoryStateV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::RepositoryState,
            actual: object_kind(&object),
        });
    };
    let retained =
        |id: &wire::RetainedSetId| digest_domain(&id.digest).map(crate::RetainedSetId::from_digest);
    Ok(crate::RepositoryStateRecord {
        base_view: crate::ViewId::from_digest(digest_domain(&v.base_view.digest)?),
        working_view: crate::ViewId::from_digest(digest_domain(&v.working_view.digest)?),
        published_view: crate::ViewId::from_digest(digest_domain(&v.published_view.digest)?),
        required_content: ContentManifestId::from_digest(digest_domain(
            &v.required_content.digest,
        )?),
        retained_changes: retained(&v.retained_changes)?,
        retained_views: retained(&v.retained_views)?,
        retained_revisions: retained(&v.retained_revisions)?,
        retained_applications: retained(&v.retained_applications)?,
    })
}

pub fn decode_branch_manifest_record(
    bytes: &[u8],
) -> Result<crate::BranchManifestRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::BranchManifestV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::BranchManifest,
            actual: object_kind(&object),
        });
    };
    crate::BranchManifestRecord::new(v.branches.into_iter().map(|entry| {
        (
            entry.name,
            crate::ViewId::from_digest(
                digest_domain(&entry.view.digest).expect("validated digest"),
            ),
        )
    }))
}

pub fn decode_repository_envelope_record(
    bytes: &[u8],
) -> Result<crate::RepositoryEnvelopeRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::RepositoryEnvelopeV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::RepositoryEnvelope,
            actual: object_kind(&object),
        });
    };
    Ok(crate::RepositoryEnvelopeRecord {
        state: crate::RepositoryStateId::from_digest(digest_domain(&v.state.digest)?),
        branches: crate::BranchManifestId::from_digest(digest_domain(&v.branches.digest)?),
    })
}

pub fn validate_directory_component(name: &str) -> Result<(), CanonicalObjectError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(CanonicalObjectError::InvalidDirectoryComponent(
            name.to_owned(),
        ));
    }
    Ok(())
}

pub fn decode_directory_node(bytes: &[u8]) -> Result<DirectoryNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::TreeV1(tree) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::Tree,
            actual: object_kind(&object),
        });
    };
    let entries = tree
        .entries
        .into_iter()
        .map(|entry| {
            let value = match entry.value {
                wire::DirectoryEntryValueV1::FileEntryV1(file) => {
                    DirectoryEntry::File(content_id_domain(&file.content)?)
                }
                wire::DirectoryEntryValueV1::DirectoryEntryRefV1(directory) => {
                    DirectoryEntry::Directory(TreeId::from_digest(digest_domain(
                        &directory.directory.digest,
                    )?))
                }
            };
            Ok((entry.name, value))
        })
        .collect::<Result<BTreeMap<_, _>, CanonicalObjectError>>()?;
    Ok(DirectoryNode { entries })
}

pub fn decode_revision_map_node(bytes: &[u8]) -> Result<RevisionMapNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::RevisionMapNodeV1(node) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::RevisionMapNode,
            actual: object_kind(&object),
        });
    };
    Ok(RevisionMapNode {
        depth: node.depth,
        value: node.value.first().map(|value| {
            RevisionId::from_digest(digest_domain(&value.digest).expect("validated digest"))
        }),
        children: node
            .children
            .iter()
            .map(|child| RevisionMapChild {
                edge: child.edge,
                child: crate::RevisionMapId::from_digest(
                    digest_domain(&child.child.digest).expect("validated digest"),
                ),
            })
            .collect(),
    })
}

pub(crate) fn validate_revision_map_node(
    node: &RevisionMapNode,
) -> Result<(), CanonicalObjectError> {
    if node.depth > 16 {
        return Err(CanonicalObjectError::InvalidCollection(
            "revision map depth exceeds key length",
        ));
    }
    if node.value.is_some() && node.depth != 16 {
        return Err(CanonicalObjectError::InvalidCollection(
            "revision map value requires terminal depth",
        ));
    }
    if node.depth == 16 && !node.children.is_empty() {
        return Err(CanonicalObjectError::InvalidCollection(
            "terminal revision map node cannot have children",
        ));
    }
    validate_sorted_unique_by(&node.children, |child| &child.edge, "revision map children")
}

pub(crate) fn revision_map_node_bytes(node: &RevisionMapNode) -> Vec<u8> {
    validate_revision_map_node(node).expect("revision map node violates canonical invariants");
    encode(wire::WheatObject::RevisionMapNodeV1(
        wire::RevisionMapNodeV1Input {
            depth: node.depth,
            value: node.value.into_iter().map(revision_id).collect(),
            children: node
                .children
                .iter()
                .map(|child| wire::RevisionMapChildV1 {
                    edge: child.edge,
                    child: revision_map_id(child.child),
                })
                .collect(),
        }
        .into(),
    ))
}

macro_rules! decode_set_node {
    ($function:ident, $variant:ident, $kind:ident, $node:ident, $child:ident, $id:ty) => {
        pub fn $function(bytes: &[u8]) -> Result<$node, CanonicalObjectError> {
            let object = decode_validated(bytes)?;
            let wire::WheatObject::$variant(node) = object else {
                return Err(CanonicalObjectError::WrongKind {
                    expected: CanonicalObjectKind::$kind,
                    actual: object_kind(&object),
                });
            };
            Ok($node {
                depth: node.depth,
                present: node.present,
                children: node
                    .children
                    .into_iter()
                    .map(|child| $child {
                        edge: child.edge,
                        child: <$id>::from_digest(
                            digest_domain(&child.child.digest).expect("validated digest"),
                        ),
                    })
                    .collect(),
            })
        }
    };
}

decode_set_node!(
    decode_conflict_set_node,
    ConflictSetNodeV1,
    ConflictSetNode,
    ConflictSetNode,
    ConflictSetChild,
    crate::ConflictSetId
);
decode_set_node!(
    decode_content_set_node,
    ContentSetNodeV1,
    ContentSetNode,
    ContentSetNode,
    ContentSetChild,
    crate::ContentSetId
);
decode_set_node!(
    decode_retained_set_node,
    RetainedSetNodeV1,
    RetainedSetNode,
    RetainedSetNode,
    RetainedSetChild,
    crate::RetainedSetId
);

pub fn decode_application_map_node(
    bytes: &[u8],
) -> Result<ApplicationMapNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ApplicationMapNodeV1(node) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ApplicationMapNode,
            actual: object_kind(&object),
        });
    };
    Ok(ApplicationMapNode {
        depth: node.depth,
        value: node.value.first().map(|value| {
            crate::ApplicationId::from_digest(
                digest_domain(&value.digest).expect("validated digest"),
            )
        }),
        children: node
            .children
            .into_iter()
            .map(|child| ApplicationMapChild {
                edge: child.edge,
                child: crate::ApplicationMapId::from_digest(
                    digest_domain(&child.child.digest).expect("validated digest"),
                ),
            })
            .collect(),
    })
}

fn validate_trie(
    depth: u8,
    terminal: bool,
    children: &[u8],
    key_len: u8,
    name: &'static str,
) -> Result<(), CanonicalObjectError> {
    if depth > key_len {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    if terminal && depth != key_len {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    if depth == key_len && !children.is_empty() {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    validate_sorted_unique_by(children, |edge| edge, name)
}

pub(crate) fn validate_application_map_node(
    node: &ApplicationMapNode,
) -> Result<(), CanonicalObjectError> {
    validate_trie(
        node.depth,
        node.value.is_some(),
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        16,
        "application map node",
    )
}
pub(crate) fn validate_conflict_set_node(
    node: &ConflictSetNode,
) -> Result<(), CanonicalObjectError> {
    validate_trie(
        node.depth,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        32,
        "conflict set node",
    )
}
pub(crate) fn validate_content_set_node(node: &ContentSetNode) -> Result<(), CanonicalObjectError> {
    validate_trie(
        node.depth,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        32,
        "content set node",
    )
}
pub(crate) fn validate_retained_set_node(
    node: &RetainedSetNode,
) -> Result<(), CanonicalObjectError> {
    validate_trie(
        node.depth,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        32,
        "retained set node",
    )
}

pub(crate) fn application_map_node_bytes(node: &ApplicationMapNode) -> Vec<u8> {
    validate_application_map_node(node)
        .expect("application map node violates canonical invariants");
    encode(wire::WheatObject::ApplicationMapNodeV1(
        wire::ApplicationMapNodeV1Input {
            depth: node.depth,
            value: node.value.into_iter().map(application_id).collect(),
            children: node
                .children
                .iter()
                .map(|c| wire::ApplicationMapChildV1 {
                    edge: c.edge,
                    child: application_map_id(c.child),
                })
                .collect(),
        }
        .into(),
    ))
}

macro_rules! set_node_bytes {
    ($function:ident, $validate:ident, $variant:ident, $input:ident, $wire_child:ident, $id_fn:ident, $node:ty) => {
        pub(crate) fn $function(node: &$node) -> Vec<u8> {
            $validate(node).expect("set node violates canonical invariants");
            encode(wire::WheatObject::$variant(
                wire::$input {
                    depth: node.depth,
                    present: node.present,
                    children: node
                        .children
                        .iter()
                        .map(|c| wire::$wire_child {
                            edge: c.edge,
                            child: $id_fn(c.child),
                        })
                        .collect(),
                }
                .into(),
            ))
        }
    };
}
set_node_bytes!(
    conflict_set_node_bytes,
    validate_conflict_set_node,
    ConflictSetNodeV1,
    ConflictSetNodeV1Input,
    ConflictSetChildV1,
    conflict_set_id,
    ConflictSetNode
);
set_node_bytes!(
    content_set_node_bytes,
    validate_content_set_node,
    ContentSetNodeV1,
    ContentSetNodeV1Input,
    ContentSetChildV1,
    content_set_id,
    ContentSetNode
);
set_node_bytes!(
    retained_set_node_bytes,
    validate_retained_set_node,
    RetainedSetNodeV1,
    RetainedSetNodeV1Input,
    RetainedSetChildV1,
    retained_set_id,
    RetainedSetNode
);

pub(crate) fn directory_node_bytes(node: &DirectoryNode) -> Vec<u8> {
    for name in node.entries.keys() {
        validate_directory_component(name).expect("directory node contains an invalid component");
    }
    encode(wire::WheatObject::TreeV1(
        wire::TreeV1Input {
            entries: node
                .entries
                .iter()
                .map(|(name, entry)| wire::DirectoryEntryV1 {
                    name: name.clone(),
                    value: match entry {
                        DirectoryEntry::File(content) => wire::DirectoryEntryValueV1::FileEntryV1(
                            wire::FileEntryV1Input {
                                content: content_id(*content),
                            }
                            .into(),
                        ),
                        DirectoryEntry::Directory(directory) => {
                            wire::DirectoryEntryValueV1::DirectoryEntryRefV1(
                                wire::DirectoryEntryRefV1Input {
                                    directory: tree_id_wire(*directory),
                                }
                                .into(),
                            )
                        }
                    },
                })
                .collect(),
        }
        .into(),
    ))
}

pub(crate) fn tree_id(tree: &Tree) -> TreeId {
    tree_objects(tree)
        .pop()
        .expect("every tree has a root directory")
        .0
}

pub(crate) fn tree_objects(tree: &Tree) -> Vec<(TreeId, Vec<u8>)> {
    #[derive(Default)]
    struct PendingDirectory {
        files: BTreeMap<String, ContentId>,
        directories: BTreeMap<String, PendingDirectory>,
    }

    fn emit(directory: PendingDirectory, objects: &mut Vec<(TreeId, Vec<u8>)>) -> TreeId {
        let mut entries = directory
            .files
            .into_iter()
            .map(|(name, content)| (name, DirectoryEntry::File(content)))
            .collect::<BTreeMap<_, _>>();
        for (name, child) in directory.directories {
            entries.insert(name, DirectoryEntry::Directory(emit(child, objects)));
        }
        let node = DirectoryNode { entries };
        let bytes = directory_node_bytes(&node);
        let id = TreeId::of_canonical_bytes(&bytes);
        objects.push((id, bytes));
        id
    }

    let mut root = PendingDirectory::default();
    for (path, content) in tree {
        let mut components = path.split('/').peekable();
        let mut directory = &mut root;
        while let Some(component) = components.next() {
            validate_directory_component(component)
                .expect("flat tree contains an invalid path component");
            if components.peek().is_none() {
                assert!(
                    !directory.directories.contains_key(component),
                    "tree path is both a file and directory"
                );
                directory.files.insert(component.to_owned(), *content);
            } else {
                assert!(
                    !directory.files.contains_key(component),
                    "tree path is both a file and directory"
                );
                directory = directory
                    .directories
                    .entry(component.to_owned())
                    .or_default();
            }
        }
    }
    let mut objects = Vec::new();
    emit(root, &mut objects);
    objects
}

pub(crate) fn content_manifest_record_bytes(record: &crate::ContentManifestRecord) -> Vec<u8> {
    encode(wire::WheatObject::ContentManifestV1(
        wire::ContentManifestV1Input {
            content: content_set_id(record.content),
        }
        .into(),
    ))
}

pub(crate) fn content_manifest_bytes(content: &BTreeSet<ContentId>) -> Vec<u8> {
    let content = crate::build_content_set(content.iter().copied(), |_| {});
    content_manifest_record_bytes(&crate::ContentManifestRecord { content })
}

pub(crate) fn content_manifest_id(content: &BTreeSet<ContentId>) -> ContentManifestId {
    ContentManifestId::of_canonical_bytes(&content_manifest_bytes(content))
}

pub(crate) fn change_bytes(change: &Change) -> Vec<u8> {
    encode(wire::WheatObject::ChangeV1(
        wire::ChangeV1Input {
            change_id: change_id(change.id),
            metadata: change_metadata(&change.metadata),
        }
        .into(),
    ))
}

pub(crate) fn revision_bytes(revision: &ChangeRevision) -> Vec<u8> {
    encode(wire::WheatObject::RevisionV1(
        wire::RevisionV1Input {
            change_id: change_id(revision.change_id),
            metadata: revision_metadata(&revision.metadata),
            predecessors: revision.predecessor.into_iter().map(revision_id).collect(),
            dependencies: revision
                .dependencies
                .iter()
                .copied()
                .map(change_id)
                .collect(),
            authored_against: view_id(revision.authored_against),
            transformation: transformation(&revision.transformation),
            resulting_tree: tree_id_wire(tree_id(&revision.resulting_tree)),
        }
        .into(),
    ))
}

pub(crate) fn conflict_bytes(conflict: &Conflict) -> Vec<u8> {
    encode(wire::WheatObject::ConflictV1(
        wire::ConflictV1Input {
            target_view: view_id(conflict.base_view),
            paths: conflict.paths.iter().map(path_conflict).collect(),
        }
        .into(),
    ))
}

pub(crate) fn application_bytes(application: &Application) -> Vec<u8> {
    let result = match &application.result {
        ApplicationResult::Clean { tree } => wire::ApplicationResultV1::CleanApplicationV1(
            wire::CleanApplicationV1Input {
                resulting_tree: tree_id_wire(tree_id(tree)),
            }
            .into(),
        ),
        ApplicationResult::Conflicted {
            partial_tree,
            conflict,
        } => {
            assert_eq!(
                conflict.id,
                crate::ConflictId::of_canonical_bytes(&conflict_bytes(conflict)),
                "application conflict ID must match its canonical content"
            );
            wire::ApplicationResultV1::ConflictedApplicationV1(
                wire::ConflictedApplicationV1Input {
                    partial_tree: tree_id_wire(tree_id(partial_tree)),
                    conflict: conflict_id(conflict.id),
                }
                .into(),
            )
        }
    };
    encode(wire::WheatObject::ApplicationV1(
        wire::ApplicationV1Input {
            revision_id: revision_id(application.revision_id),
            target_view: view_id(application.target_view),
            result,
        }
        .into(),
    ))
}

pub(crate) fn branch_manifest_record_bytes(record: &crate::BranchManifestRecord) -> Vec<u8> {
    encode(wire::WheatObject::BranchManifestV1(
        wire::BranchManifestV1Input {
            branches: record
                .branches
                .iter()
                .map(|(name, view)| wire::BranchManifestEntryV1 {
                    name: name.clone(),
                    view: view_id(*view),
                })
                .collect(),
        }
        .into(),
    ))
}

pub(crate) fn repository_envelope_record_bytes(
    record: &crate::RepositoryEnvelopeRecord,
) -> Vec<u8> {
    encode(wire::WheatObject::RepositoryEnvelopeV1(
        wire::RepositoryEnvelopeV1Input {
            state: repository_state_id(record.state),
            branches: branch_manifest_id(record.branches),
        }
        .into(),
    ))
}

pub(crate) fn repository_state_record_bytes(record: &crate::RepositoryStateRecord) -> Vec<u8> {
    encode(wire::WheatObject::RepositoryStateV1(
        wire::RepositoryStateV1Input {
            base_view: view_id(record.base_view),
            working_view: view_id(record.working_view),
            published_view: view_id(record.published_view),
            required_content: content_manifest_id_wire(record.required_content),
            retained_changes: retained_set_id(record.retained_changes),
            retained_views: retained_set_id(record.retained_views),
            retained_revisions: retained_set_id(record.retained_revisions),
            retained_applications: retained_set_id(record.retained_applications),
        }
        .into(),
    ))
}

pub(crate) fn repository_state_bytes(state: &RepositoryState) -> Vec<u8> {
    crate::build_repository_state_record(state, &mut |_| {}).canonical_bytes()
}

pub(crate) fn view_record_bytes(record: &crate::ViewRecord) -> Vec<u8> {
    encode(wire::WheatObject::ViewV1(
        wire::ViewV1Input {
            tree: tree_id_wire(record.tree),
            revisions: revision_map_id(record.revisions),
            applications: application_map_id(record.applications),
            unresolved_conflicts: conflict_set_id(record.unresolved_conflicts),
            required_content: content_manifest_id_wire(record.required_content),
        }
        .into(),
    ))
}

pub(crate) fn view_bytes(view: &RepositoryView) -> Vec<u8> {
    crate::build_view_record(view, &mut |_| {}).canonical_bytes()
}

fn validate_object(object: &wire::WheatObject) -> Result<(), CanonicalObjectError> {
    match object {
        wire::WheatObject::TreeV1(tree) => {
            validate_sorted_unique_by(
                &tree.entries,
                |entry| entry.name.as_str(),
                "directory entries",
            )?;
            for entry in &tree.entries {
                validate_directory_component(&entry.name)?;
                if let wire::DirectoryEntryValueV1::FileEntryV1(file) = &entry.value {
                    validate_content_id(&file.content)?;
                }
            }
        }
        wire::WheatObject::ChangeV1(_) => {}
        wire::WheatObject::RevisionV1(revision) => {
            if revision.predecessors.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision must have at most one predecessor",
                ));
            }
            validate_sorted_unique_by(
                &revision.dependencies,
                |dependency| dependency.uuid.bytes.as_slice(),
                "revision dependencies",
            )?;
            validate_sorted_unique_by(
                &revision.transformation.edits,
                |edit| edit.path.as_str(),
                "transformation paths",
            )?;
            for edit in &revision.transformation.edits {
                validate_optional_content_id(edit.expected.as_ref())?;
                validate_optional_content_id(edit.replacement.as_ref())?;
            }
        }
        wire::WheatObject::ApplicationV1(_) => {}
        wire::WheatObject::ConflictV1(conflict) => {
            validate_sorted_unique_by(
                &conflict.paths,
                |path| path.path.as_str(),
                "conflict paths",
            )?;
            for path in &conflict.paths {
                validate_optional_content_id(path.expected.as_ref())?;
                validate_optional_content_id(path.actual.as_ref())?;
                validate_optional_content_id(path.proposed.as_ref())?;
            }
        }
        wire::WheatObject::ViewV1(_) => {}
        wire::WheatObject::ContentManifestV1(_) => {}
        wire::WheatObject::RevisionMapNodeV1(node) => {
            if node.depth > 16 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision map depth exceeds key length",
                ));
            }
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision map must have at most one value",
                ));
            }
            if !node.value.is_empty() && node.depth != 16 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision map value requires terminal depth",
                ));
            }
            if node.depth == 16 && !node.children.is_empty() {
                return Err(CanonicalObjectError::InvalidCollection(
                    "terminal revision map node cannot have children",
                ));
            }
            validate_sorted_unique_by(
                &node.children,
                |child| &child.edge,
                "revision map children",
            )?;
        }
        wire::WheatObject::ApplicationMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "application map value",
                ));
            }
            validate_trie(
                node.depth,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                16,
                "application map node",
            )?;
        }
        wire::WheatObject::ConflictSetNodeV1(node) => {
            validate_trie(
                node.depth,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                32,
                "conflict set node",
            )?;
        }
        wire::WheatObject::ContentSetNodeV1(node) => {
            validate_trie(
                node.depth,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                32,
                "content set node",
            )?;
        }
        wire::WheatObject::RetainedSetNodeV1(node) => {
            validate_trie(
                node.depth,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                32,
                "retained set node",
            )?;
        }
        wire::WheatObject::RepositoryStateV1(_) | wire::WheatObject::RepositoryEnvelopeV1(_) => {}
        wire::WheatObject::BranchManifestV1(manifest) => {
            validate_sorted_unique_by(
                &manifest.branches,
                |entry| entry.name.as_str(),
                "branch manifest entries",
            )?;
            for entry in &manifest.branches {
                crate::branch_records::validate_branch_name(&entry.name)?;
            }
        }
    }
    Ok(())
}

fn object_kind(object: &wire::WheatObject) -> CanonicalObjectKind {
    match object {
        wire::WheatObject::TreeV1(_) => CanonicalObjectKind::Tree,
        wire::WheatObject::ChangeV1(_) => CanonicalObjectKind::Change,
        wire::WheatObject::RevisionV1(_) => CanonicalObjectKind::Revision,
        wire::WheatObject::ApplicationV1(_) => CanonicalObjectKind::Application,
        wire::WheatObject::ConflictV1(_) => CanonicalObjectKind::Conflict,
        wire::WheatObject::ViewV1(_) => CanonicalObjectKind::View,
        wire::WheatObject::ContentManifestV1(_) => CanonicalObjectKind::ContentManifest,
        wire::WheatObject::RepositoryStateV1(_) => CanonicalObjectKind::RepositoryState,
        wire::WheatObject::BranchManifestV1(_) => CanonicalObjectKind::BranchManifest,
        wire::WheatObject::RepositoryEnvelopeV1(_) => CanonicalObjectKind::RepositoryEnvelope,
        wire::WheatObject::RevisionMapNodeV1(_) => CanonicalObjectKind::RevisionMapNode,
        wire::WheatObject::ApplicationMapNodeV1(_) => CanonicalObjectKind::ApplicationMapNode,
        wire::WheatObject::ConflictSetNodeV1(_) => CanonicalObjectKind::ConflictSetNode,
        wire::WheatObject::ContentSetNodeV1(_) => CanonicalObjectKind::ContentSetNode,
        wire::WheatObject::RetainedSetNodeV1(_) => CanonicalObjectKind::RetainedSetNode,
    }
}

fn validate_sorted_unique_by<T, K: Ord + ?Sized>(
    values: &[T],
    key: impl Fn(&T) -> &K,
    name: &'static str,
) -> Result<(), CanonicalObjectError> {
    if values
        .windows(2)
        .all(|values| key(&values[0]) < key(&values[1]))
    {
        Ok(())
    } else {
        Err(CanonicalObjectError::InvalidCollection(name))
    }
}

fn validate_optional_content_id(
    content: Option<&wire::ContentId>,
) -> Result<(), CanonicalObjectError> {
    content.map_or(Ok(()), validate_content_id)
}

fn validate_content_id(content: &wire::ContentId) -> Result<(), CanonicalObjectError> {
    if content.algorithm == BLAKE3_256 {
        Ok(())
    } else {
        Err(CanonicalObjectError::UnsupportedContentAlgorithm(
            content.algorithm,
        ))
    }
}

fn content_id_domain(content: &wire::ContentId) -> Result<ContentId, CanonicalObjectError> {
    validate_content_id(content)?;
    Ok(ContentId::from_digest(digest_domain(&content.digest)?))
}

fn digest_domain(digest: &wire::Digest256) -> Result<[u8; 32], CanonicalObjectError> {
    digest
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| CanonicalObjectError::Decode("digest is not 32 bytes".into()))
}

fn actor(value: &Actor) -> wire::ActorV1 {
    wire::ActorV1 {
        name: value.name.clone(),
        email: value.email.clone(),
    }
}

fn timestamp(value: Timestamp) -> wire::TimestampV1 {
    wire::TimestampV1 {
        seconds: value.seconds,
        offset_minutes: value.offset_minutes,
    }
}

fn provenance(value: &SourceProvenance) -> wire::SourceProvenanceV1 {
    wire::SourceProvenanceV1 {
        system: value.system.clone(),
        object_id: value.object_id.clone(),
        parent_object_ids: value
            .parent_object_ids
            .iter()
            .cloned()
            .map(|bytes| wire::SourceObjectIdV1 { bytes })
            .collect(),
    }
}

fn change_metadata(value: &ChangeMetadata) -> wire::ChangeMetadataV1 {
    wire::ChangeMetadataV1 {
        title: value.title.clone(),
        description: value.description.clone(),
        creator: actor(&value.creator),
        created_at: timestamp(value.created_at),
        provenance: value.provenance.as_ref().map(provenance),
    }
}

fn revision_metadata(value: &RevisionMetadata) -> wire::RevisionMetadataV1 {
    wire::RevisionMetadataV1 {
        message: value.message.clone(),
        author: actor(&value.author),
        authored_at: timestamp(value.authored_at),
        committer: actor(&value.committer),
        committed_at: timestamp(value.committed_at),
        provenance: value.provenance.as_ref().map(provenance),
    }
}

fn transformation(value: &Transformation) -> wire::TransformationV1 {
    let mut edits = value.edits.iter().map(file_edit).collect::<Vec<_>>();
    edits.sort_by(|left, right| left.path.cmp(&right.path));
    assert!(
        edits.windows(2).all(|edits| edits[0].path != edits[1].path),
        "a transformation may contain only one edit per path"
    );
    wire::TransformationV1 { edits }
}

fn file_edit(value: &FileEdit) -> wire::FileEditV1 {
    wire::FileEditV1 {
        path: value.path.clone(),
        expected: value.expected.map(content_id),
        replacement: value.replacement.map(content_id),
    }
}

fn path_conflict(value: &PathConflict) -> wire::PathConflictV1 {
    wire::PathConflictV1 {
        path: value.path.clone(),
        expected: value.expected.map(content_id),
        actual: value.actual.map(content_id),
        proposed: value.proposed.map(content_id),
    }
}

fn change_id(value: ChangeId) -> wire::ChangeId {
    wire::ChangeId {
        uuid: wire::Uuid128 {
            bytes: value.as_bytes().to_vec(),
        },
    }
}

fn content_id(value: ContentId) -> wire::ContentId {
    wire::ContentId {
        algorithm: BLAKE3_256,
        digest: digest(value.as_bytes()),
    }
}

macro_rules! wire_digest_id {
    ($function:ident, $domain:ty, $wire:ident) => {
        fn $function(value: $domain) -> wire::$wire {
            wire::$wire {
                digest: digest(value.as_bytes()),
            }
        }
    };
}

wire_digest_id!(revision_id, RevisionId, RevisionId);
wire_digest_id!(revision_map_id, crate::RevisionMapId, RevisionMapId);
wire_digest_id!(
    application_map_id,
    crate::ApplicationMapId,
    ApplicationMapId
);
wire_digest_id!(conflict_set_id, crate::ConflictSetId, ConflictSetId);
wire_digest_id!(content_set_id, crate::ContentSetId, ContentSetId);
wire_digest_id!(retained_set_id, crate::RetainedSetId, RetainedSetId);
wire_digest_id!(tree_id_wire, TreeId, TreeId);
wire_digest_id!(application_id, crate::ApplicationId, ApplicationId);
wire_digest_id!(conflict_id, crate::ConflictId, ConflictId);
wire_digest_id!(view_id, crate::ViewId, ViewId);
wire_digest_id!(
    repository_state_id,
    crate::RepositoryStateId,
    RepositoryStateId
);
wire_digest_id!(
    branch_manifest_id,
    crate::BranchManifestId,
    BranchManifestId
);
wire_digest_id!(
    content_manifest_id_wire,
    ContentManifestId,
    ContentManifestId
);

fn digest(bytes: &[u8; 32]) -> wire::Digest256 {
    wire::Digest256 {
        bytes: bytes.to_vec(),
    }
}

fn encode(object: wire::WheatObject) -> Vec<u8> {
    let bytes = object
        .encode()
        .expect("validated Wheat domain values must encode");
    debug_assert!(validate_canonical_object(&bytes).is_ok());
    bytes
}
