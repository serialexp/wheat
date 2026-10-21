use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_binschema::generated as wire;

use crate::{
    Actor, Application, ApplicationMapChild, ApplicationMapNode, ApplicationResult, Change,
    ChangeId, ChangeMapChild, ChangeMapNode, ChangeMetadata, ChangeRevision, Conflict,
    ConflictSetChild, ConflictSetNode, ContentId, ContentMapChild, ContentMapId, ContentMapNode,
    ContentSetChild, ContentSetId, ContentSetNode, DirectoryEntry, DirectoryNode, EditOperation,
    EditRecord, FileKind, FileValue, ProjectedCommitSetChild, ProjectedCommitSetNode,
    RepositoryState, RepositoryView, RetainedSetChild, RetainedSetNode, RevisionId,
    RevisionMapChild, RevisionMapNode, RevisionMetadata, SourceProvenance, Timestamp, Tree, TreeId,
    ViewLineageMapChild, ViewLineageMapNode,
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
    #[error("invalid view label {0:?}")]
    InvalidViewLabel(String),
    #[error("invalid label annotation: {0}")]
    InvalidLabelAnnotation(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalObjectKind {
    Tree,
    Edit,
    Change,
    Revision,
    Application,
    Conflict,
    View,
    ContentMapNode,
    RepositoryState,
    ViewLabelManifest,
    RepositoryEnvelope,
    RevisionMapNode,
    ChangeMapNode,
    ApplicationMapNode,
    ConflictSetNode,
    ContentSetNode,
    RetainedSetNode,
    ViewLineageMapNode,
    ProjectedCommitSetNode,
}

pub fn validate_canonical_object(
    bytes: &[u8],
) -> Result<CanonicalObjectKind, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    Ok(object_kind(&object))
}

fn decode_validated(bytes: &[u8]) -> Result<wire::WheatObject, CanonicalObjectError> {
    let mut decoder =
        wheat_binschema::BitStreamDecoder::new(bytes, wheat_binschema::BitOrder::MsbFirst);
    let object = wire::WheatObject::decode_with_decoder(&mut decoder)
        .map_err(|error| CanonicalObjectError::Decode(format!("{error:?}")))?;
    if decoder.position() != decoder.bytes_len() {
        return Err(CanonicalObjectError::NonCanonicalEncoding);
    }
    validate_object(&object)?;
    Ok(object)
}

pub fn decode_edit_record(bytes: &[u8]) -> Result<EditRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::EditV1(edit) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::Edit,
            actual: object_kind(&object),
        });
    };
    let operation = match edit.operation {
        wire::EditOperationV1::GuardedTextEditV1(value) => EditOperation::GuardedText {
            kind: guarded_file_kind(value.kind.value)?,
            before: content_id_domain(&value.before)?,
            expected: content_id_domain(&value.expected)?,
            replacement: content_id_domain(&value.replacement)?,
            after: content_id_domain(&value.after)?,
            start_anchor: value.start_anchor,
            end_anchor: value.end_anchor,
        },
        wire::EditOperationV1::ExactFileEditV1(value) => EditOperation::ExactFile {
            expected: value.expected.first().map(file_value_domain).transpose()?,
            replacement: value
                .replacement
                .first()
                .map(file_value_domain)
                .transpose()?,
        },
    };
    Ok(EditRecord {
        path: edit.path,
        operation,
    })
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
        required_content: ContentMapId::from_digest(digest_domain(&v.required_content.digest)?),
        order: crate::ViewOrder {
            size: v.size,
            last: v
                .last
                .first()
                .map(crate::semantic_records::change_id_domain)
                .transpose()?,
            weight: v.weight,
        },
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
        changes: crate::ChangeMapId::from_digest(digest_domain(&v.changes.digest)?),
        view_lineage: crate::ViewLineageMapId::from_digest(digest_domain(&v.view_lineage.digest)?),
        required_content: ContentSetId::from_digest(digest_domain(&v.required_content.digest)?),
        retained_changes: retained(&v.retained_changes)?,
        retained_views: retained(&v.retained_views)?,
        retained_revisions: retained(&v.retained_revisions)?,
        retained_applications: retained(&v.retained_applications)?,
        projected_commits: crate::ProjectedCommitSetId::from_digest(digest_domain(
            &v.projected_commits.digest,
        )?),
    })
}

pub fn decode_view_label_manifest_record(
    bytes: &[u8],
) -> Result<crate::ViewLabelManifestRecord, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ViewLabelManifestV1(v) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ViewLabelManifest,
            actual: object_kind(&object),
        });
    };
    crate::ViewLabelManifestRecord::new(v.labels.into_iter().map(|entry| {
        (
            entry.name,
            crate::ViewLabel {
                view: crate::ViewId::from_digest(
                    digest_domain(&entry.view.digest).expect("validated digest"),
                ),
                original_git_ref: entry.original_git_refs.into_iter().next(),
                annotation: entry.annotation.map(|annotation| crate::LabelAnnotation {
                    tagger: Actor {
                        name: annotation.tagger.name,
                        email: annotation.tagger.email,
                    },
                    tagged_at: Timestamp {
                        seconds: annotation.tagged_at.seconds,
                        offset_minutes: annotation.tagged_at.offset_minutes,
                    },
                    message: annotation.message,
                }),
            },
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
        labels: crate::ViewLabelManifestId::from_digest(digest_domain(&v.labels.digest)?),
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
                    DirectoryEntry::File(file_value_domain(&file.value)?)
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
        prefix: node.prefix,
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
        entries: node
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.suffix.clone(),
                    RevisionId::from_digest(
                        digest_domain(&entry.value.digest).expect("validated digest"),
                    ),
                )
            })
            .collect(),
    })
}

pub(crate) fn validate_revision_map_node(
    node: &RevisionMapNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|(suffix, _)| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.value.is_some(),
        &node
            .children
            .iter()
            .map(|child| child.edge)
            .collect::<Vec<_>>(),
        &entry_first_bytes,
        16,
        "revision map node",
    )
}

pub(crate) fn revision_map_node_bytes(node: &RevisionMapNode) -> Vec<u8> {
    validate_revision_map_node(node).expect("revision map node violates canonical invariants");
    encode(wire::WheatObject::RevisionMapNodeV1(
        wire::RevisionMapNodeV1Input {
            prefix: node.prefix.clone(),
            value: node.value.into_iter().map(revision_id).collect(),
            children: node
                .children
                .iter()
                .map(|child| wire::RevisionMapChildV1 {
                    edge: child.edge,
                    child: revision_map_id(child.child),
                })
                .collect(),
            entries: node
                .entries
                .iter()
                .map(|(suffix, value)| wire::RevisionMapEntryV1 {
                    suffix: suffix.clone(),
                    value: revision_id(*value),
                })
                .collect(),
        }
        .into(),
    ))
}

pub fn decode_change_map_node(bytes: &[u8]) -> Result<ChangeMapNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ChangeMapNodeV1(node) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ChangeMapNode,
            actual: object_kind(&object),
        });
    };
    Ok(ChangeMapNode {
        prefix: node.prefix,
        value: node.value.first().map(|value| {
            crate::ChangeObjectId::from_digest(
                digest_domain(&value.digest).expect("validated digest"),
            )
        }),
        children: node
            .children
            .iter()
            .map(|child| ChangeMapChild {
                edge: child.edge,
                child: crate::ChangeMapId::from_digest(
                    digest_domain(&child.child.digest).expect("validated digest"),
                ),
            })
            .collect(),
        entries: node
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.suffix.clone(),
                    crate::ChangeObjectId::from_digest(
                        digest_domain(&entry.value.digest).expect("validated digest"),
                    ),
                )
            })
            .collect(),
    })
}

pub(crate) fn validate_change_map_node(node: &ChangeMapNode) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|(suffix, _)| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.value.is_some(),
        &node
            .children
            .iter()
            .map(|child| child.edge)
            .collect::<Vec<_>>(),
        &entry_first_bytes,
        16,
        "change map node",
    )
}

pub(crate) fn change_map_node_bytes(node: &ChangeMapNode) -> Vec<u8> {
    validate_change_map_node(node).expect("change map node violates canonical invariants");
    encode(wire::WheatObject::ChangeMapNodeV1(
        wire::ChangeMapNodeV1Input {
            prefix: node.prefix.clone(),
            value: node.value.into_iter().map(change_object_id).collect(),
            children: node
                .children
                .iter()
                .map(|child| wire::ChangeMapChildV1 {
                    edge: child.edge,
                    child: change_map_id(child.child),
                })
                .collect(),
            entries: node
                .entries
                .iter()
                .map(|(suffix, value)| wire::ChangeMapEntryV1 {
                    suffix: suffix.clone(),
                    value: change_object_id(*value),
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
                prefix: node.prefix,
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
                entries: node.entries.into_iter().map(|entry| entry.suffix).collect(),
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
    ContentSetId
);
decode_set_node!(
    decode_retained_set_node,
    RetainedSetNodeV1,
    RetainedSetNode,
    RetainedSetNode,
    RetainedSetChild,
    crate::RetainedSetId
);
decode_set_node!(
    decode_projected_commit_set_node,
    ProjectedCommitSetNodeV1,
    ProjectedCommitSetNode,
    ProjectedCommitSetNode,
    ProjectedCommitSetChild,
    crate::ProjectedCommitSetId
);

pub fn decode_content_map_node(bytes: &[u8]) -> Result<ContentMapNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ContentMapNodeV1(node) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ContentMapNode,
            actual: object_kind(&object),
        });
    };
    Ok(ContentMapNode {
        prefix: node.prefix,
        value: node.value.first().copied(),
        children: node
            .children
            .into_iter()
            .map(|child| ContentMapChild {
                edge: child.edge,
                child: ContentMapId::from_digest(
                    digest_domain(&child.child.digest).expect("validated digest"),
                ),
            })
            .collect(),
        entries: node
            .entries
            .into_iter()
            .map(|entry| (entry.suffix, entry.value))
            .collect(),
    })
}

pub fn decode_view_lineage_map_node(
    bytes: &[u8],
) -> Result<ViewLineageMapNode, CanonicalObjectError> {
    let object = decode_validated(bytes)?;
    let wire::WheatObject::ViewLineageMapNodeV1(node) = object else {
        return Err(CanonicalObjectError::WrongKind {
            expected: CanonicalObjectKind::ViewLineageMapNode,
            actual: object_kind(&object),
        });
    };
    Ok(ViewLineageMapNode {
        prefix: node.prefix,
        value: node.value.first().map(|value| {
            crate::RetainedSetId::from_digest(
                digest_domain(&value.digest).expect("validated digest"),
            )
        }),
        children: node
            .children
            .into_iter()
            .map(|child| ViewLineageMapChild {
                edge: child.edge,
                child: crate::ViewLineageMapId::from_digest(
                    digest_domain(&child.child.digest).expect("validated digest"),
                ),
            })
            .collect(),
        entries: node
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.suffix.clone(),
                    crate::RetainedSetId::from_digest(
                        digest_domain(&entry.value.digest).expect("validated digest"),
                    ),
                )
            })
            .collect(),
    })
}

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
        prefix: node.prefix,
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
        entries: node
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.suffix.clone(),
                    crate::ApplicationId::from_digest(
                        digest_domain(&entry.value.digest).expect("validated digest"),
                    ),
                )
            })
            .collect(),
    })
}

fn validate_radix_node(
    prefix: &[u8],
    terminal: bool,
    children: &[u8],
    entry_first_bytes: &[u8],
    key_len: usize,
    name: &'static str,
) -> Result<(), CanonicalObjectError> {
    let total_branches = children.len() + entry_first_bytes.len();
    if prefix.len() > key_len {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    // A terminal node (value at this position) must not have children or entries
    // — it consumes the full key.
    if terminal && total_branches != 0 {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    // Non-terminal, non-root: must have ≥2 branches (children + entries), or
    // be a root empty node (empty prefix, no value, no branches).
    if !terminal && total_branches == 1 {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    if !terminal && total_branches == 0 && !prefix.is_empty() {
        return Err(CanonicalObjectError::InvalidCollection(name));
    }
    validate_sorted_unique_by(children, |edge| edge, name)?;
    validate_sorted_unique_by(entry_first_bytes, |b| b, name)?;
    // Children edges and entry first bytes must be disjoint.
    let mut ci = 0;
    let mut ei = 0;
    while ci < children.len() && ei < entry_first_bytes.len() {
        if children[ci] == entry_first_bytes[ei] {
            return Err(CanonicalObjectError::InvalidCollection(name));
        }
        if children[ci] < entry_first_bytes[ei] {
            ci += 1;
        } else {
            ei += 1;
        }
    }
    Ok(())
}

pub(crate) fn validate_view_lineage_map_node(
    node: &ViewLineageMapNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|(suffix, _)| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.value.is_some(),
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        32,
        "view lineage map node",
    )
}

pub(crate) fn validate_application_map_node(
    node: &ApplicationMapNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|(suffix, _)| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.value.is_some(),
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        16,
        "application map node",
    )
}
pub(crate) fn validate_conflict_set_node(
    node: &ConflictSetNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|suffix| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        32,
        "conflict set node",
    )
}
/// A content map holds counts of at least 1: a blob no tree entry names is absent.
pub(crate) fn validate_content_map_node(node: &ContentMapNode) -> Result<(), CanonicalObjectError> {
    if node.value == Some(0) || node.entries.iter().any(|(_, count)| *count == 0) {
        return Err(CanonicalObjectError::InvalidCollection("content map node"));
    }
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|(suffix, _)| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.value.is_some(),
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        32,
        "content map node",
    )
}
pub(crate) fn validate_content_set_node(node: &ContentSetNode) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|suffix| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        32,
        "content set node",
    )
}
pub(crate) fn validate_retained_set_node(
    node: &RetainedSetNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|suffix| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        32,
        "retained set node",
    )
}
pub(crate) fn validate_projected_commit_set_node(
    node: &ProjectedCommitSetNode,
) -> Result<(), CanonicalObjectError> {
    let entry_first_bytes: Vec<u8> = node
        .entries
        .iter()
        .filter_map(|suffix| suffix.first().copied())
        .collect();
    validate_radix_node(
        &node.prefix,
        node.present,
        &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
        &entry_first_bytes,
        crate::ProjectedCommit::LEN,
        "projected commit set node",
    )
}

pub(crate) fn view_lineage_map_node_bytes(node: &ViewLineageMapNode) -> Vec<u8> {
    validate_view_lineage_map_node(node)
        .expect("view lineage map node violates canonical invariants");
    encode(wire::WheatObject::ViewLineageMapNodeV1(
        wire::ViewLineageMapNodeV1Input {
            prefix: node.prefix.clone(),
            value: node.value.into_iter().map(retained_set_id).collect(),
            children: node
                .children
                .iter()
                .map(|c| wire::ViewLineageMapChildV1 {
                    edge: c.edge,
                    child: view_lineage_map_id(c.child),
                })
                .collect(),
            entries: node
                .entries
                .iter()
                .map(|(suffix, value)| wire::ViewLineageMapEntryV1 {
                    suffix: suffix.clone(),
                    value: retained_set_id(*value),
                })
                .collect(),
        }
        .into(),
    ))
}

pub(crate) fn application_map_node_bytes(node: &ApplicationMapNode) -> Vec<u8> {
    validate_application_map_node(node)
        .expect("application map node violates canonical invariants");
    encode(wire::WheatObject::ApplicationMapNodeV1(
        wire::ApplicationMapNodeV1Input {
            prefix: node.prefix.clone(),
            value: node.value.into_iter().map(application_id).collect(),
            children: node
                .children
                .iter()
                .map(|c| wire::ApplicationMapChildV1 {
                    edge: c.edge,
                    child: application_map_id(c.child),
                })
                .collect(),
            entries: node
                .entries
                .iter()
                .map(|(suffix, value)| wire::ApplicationMapEntryV1 {
                    suffix: suffix.clone(),
                    value: application_id(*value),
                })
                .collect(),
        }
        .into(),
    ))
}

pub(crate) fn content_map_node_bytes(node: &ContentMapNode) -> Vec<u8> {
    validate_content_map_node(node).expect("content map node violates canonical invariants");
    encode(wire::WheatObject::ContentMapNodeV1(
        wire::ContentMapNodeV1Input {
            prefix: node.prefix.clone(),
            value: node.value.into_iter().collect(),
            children: node
                .children
                .iter()
                .map(|c| wire::ContentMapChildV1 {
                    edge: c.edge,
                    child: content_map_id(c.child),
                })
                .collect(),
            entries: node
                .entries
                .iter()
                .map(|(suffix, value)| wire::ContentMapEntryV1 {
                    suffix: suffix.clone(),
                    value: *value,
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
                    prefix: node.prefix.clone(),
                    present: node.present,
                    children: node
                        .children
                        .iter()
                        .map(|c| wire::$wire_child {
                            edge: c.edge,
                            child: $id_fn(c.child),
                        })
                        .collect(),
                    entries: node
                        .entries
                        .iter()
                        .map(|suffix| wire::SetEntryV1 {
                            suffix: suffix.clone(),
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
set_node_bytes!(
    projected_commit_set_node_bytes,
    validate_projected_commit_set_node,
    ProjectedCommitSetNodeV1,
    ProjectedCommitSetNodeV1Input,
    ProjectedCommitSetChildV1,
    projected_commit_set_id,
    ProjectedCommitSetNode
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
                        DirectoryEntry::File(value) => wire::DirectoryEntryValueV1::FileEntryV1(
                            wire::FileEntryV1Input {
                                value: file_value(*value),
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
        files: BTreeMap<String, FileValue>,
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
            combines: revision.combines.iter().copied().map(revision_id).collect(),
            generation: revision.generation,
            dependencies: revision
                .dependencies
                .iter()
                .copied()
                .map(change_id)
                .collect(),
            authored_against: view_id(revision.authored_against),
            edits: revision.edits.edits.keys().copied().map(edit_id).collect(),
            transition: edit_transition(&revision.edits.transition),
            resulting_tree: tree_id_wire(tree_id(&revision.resulting_tree)),
        }
        .into(),
    ))
}

fn edit_transition(value: &crate::EditTransition) -> wire::EditTransitionV1 {
    let mut subsumed = value
        .subsumed
        .iter()
        .map(|s| wire::EditSubsumptionV1 {
            source: edit_id(s.source),
            replacements: s.replacements.iter().copied().map(edit_id).collect(),
        })
        .collect::<Vec<_>>();
    subsumed.sort_by(|a, b| a.source.digest.bytes.cmp(&b.source.digest.bytes));
    let mut resolutions = value
        .resolutions
        .iter()
        .map(|r| wire::ConflictResolutionEvidenceV1 {
            application: application_id(r.application),
            conflicted_edits: r.conflicted_edits.iter().copied().map(edit_id).collect(),
            replacement_edits: r.replacement_edits.iter().copied().map(edit_id).collect(),
        })
        .collect::<Vec<_>>();
    resolutions.sort_by(|a, b| a.application.digest.bytes.cmp(&b.application.digest.bytes));
    wire::EditTransitionV1 {
        preserved: value.preserved.iter().copied().map(edit_id).collect(),
        added: value.added.iter().copied().map(edit_id).collect(),
        removed: value.removed.iter().copied().map(edit_id).collect(),
        subsumed,
        resolutions,
    }
}

fn applied_evidence(value: &crate::AppliedEditEvidence) -> wire::AppliedEditEvidenceV1 {
    let range = |r: crate::ByteRange| wire::ByteRangeV1 {
        start: r.start,
        end: r.end,
    };
    wire::AppliedEditEvidenceV1 {
        target: value.target.map(file_value).into_iter().collect(),
        read: value.read.map(range).into_iter().collect(),
        write: value.write.map(range).into_iter().collect(),
    }
}

fn failed_evidence(value: &crate::FailedEditEvidence) -> wire::FailedEditEvidenceV1 {
    let reason = match &value.reason {
        crate::EditFailure::NoMatch => {
            wire::EditFailureV1::NoMatchFailureV1(wire::NoMatchFailureV1Input {}.into())
        }
        crate::EditFailure::Ambiguous => {
            wire::EditFailureV1::AmbiguousFailureV1(wire::AmbiguousFailureV1Input {}.into())
        }
        crate::EditFailure::Interference { with } => wire::EditFailureV1::InterferenceFailureV1(
            wire::InterferenceFailureV1Input {
                with: with.iter().copied().map(edit_id).collect(),
            }
            .into(),
        ),
        crate::EditFailure::KindMismatch => {
            wire::EditFailureV1::KindMismatchFailureV1(wire::KindMismatchFailureV1Input {}.into())
        }
        crate::EditFailure::ExpectedFileMismatch => {
            wire::EditFailureV1::ExpectedFileMismatchFailureV1(
                wire::ExpectedFileMismatchFailureV1Input {}.into(),
            )
        }
    };
    wire::FailedEditEvidenceV1 {
        target: value.target.map(file_value).into_iter().collect(),
        reason,
    }
}

pub(crate) fn conflict_bytes(conflict: &Conflict) -> Vec<u8> {
    encode(wire::WheatObject::ConflictV1(
        wire::ConflictV1Input {
            target_view: view_id(conflict.base_view),
            edits: conflict
                .edits
                .iter()
                .map(|(edit, evidence)| wire::FailedEditEntryV1 {
                    edit: edit_id(*edit),
                    evidence: failed_evidence(evidence),
                })
                .collect(),
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
            position: application.order.position,
            previous: application
                .order
                .previous
                .into_iter()
                .map(application_id)
                .collect(),
            skip: application
                .order
                .skip
                .iter()
                .copied()
                .map(application_id)
                .collect(),
            window_max: application
                .order
                .window_max
                .iter()
                .copied()
                .map(change_id)
                .collect(),
            result,
            applied: application
                .applied
                .iter()
                .map(|(edit, evidence)| wire::AppliedEditEntryV1 {
                    edit: edit_id(*edit),
                    evidence: applied_evidence(evidence),
                })
                .collect(),
        }
        .into(),
    ))
}

pub(crate) fn view_label_manifest_record_bytes(record: &crate::ViewLabelManifestRecord) -> Vec<u8> {
    encode(wire::WheatObject::ViewLabelManifestV1(
        wire::ViewLabelManifestV1Input {
            labels: record
                .labels
                .iter()
                .map(|(name, label)| wire::ViewLabelManifestEntryV1 {
                    name: name.clone(),
                    view: view_id(label.view),
                    original_git_refs: label.original_git_ref.iter().cloned().collect(),
                    annotation: label.annotation.as_ref().map(|annotation| {
                        wire::LabelAnnotationV1 {
                            tagger: actor(&annotation.tagger),
                            tagged_at: timestamp(annotation.tagged_at),
                            message: annotation.message.clone(),
                        }
                    }),
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
            labels: view_label_manifest_id(record.labels),
        }
        .into(),
    ))
}

pub(crate) fn repository_state_record_bytes(record: &crate::RepositoryStateRecord) -> Vec<u8> {
    encode(wire::WheatObject::RepositoryStateV1(
        wire::RepositoryStateV1Input {
            base_view: view_id(record.base_view),
            changes: change_map_id(record.changes),
            view_lineage: view_lineage_map_id(record.view_lineage),
            required_content: content_set_id(record.required_content),
            retained_changes: retained_set_id(record.retained_changes),
            retained_views: retained_set_id(record.retained_views),
            retained_revisions: retained_set_id(record.retained_revisions),
            retained_applications: retained_set_id(record.retained_applications),
            projected_commits: projected_commit_set_id(record.projected_commits),
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
            required_content: content_map_id(record.required_content),
            size: record.order.size,
            last: record.order.last.into_iter().map(change_id).collect(),
            weight: record.order.weight,
        }
        .into(),
    ))
}

pub(crate) fn view_bytes(view: &RepositoryView) -> Vec<u8> {
    crate::build_view_record(view, &mut |_| {}).canonical_bytes()
}

pub(crate) fn validate_edit_record(edit: &EditRecord) -> Result<(), CanonicalObjectError> {
    validate_path(&edit.path)?;
    match &edit.operation {
        EditOperation::GuardedText { kind, .. } if *kind == FileKind::Symlink => Err(
            CanonicalObjectError::Decode("guarded text requires regular or executable file".into()),
        ),
        EditOperation::ExactFile {
            expected,
            replacement,
        } if expected == replacement => Err(CanonicalObjectError::InvalidCollection(
            "exact file edit must change value",
        )),
        _ => Ok(()),
    }
}

pub(crate) fn edit_record_bytes(edit: &EditRecord) -> Vec<u8> {
    validate_edit_record(edit).expect("invalid edit record");
    let operation = match &edit.operation {
        EditOperation::GuardedText {
            kind,
            before,
            expected,
            replacement,
            after,
            start_anchor,
            end_anchor,
        } => wire::EditOperationV1::GuardedTextEditV1(
            wire::GuardedTextEditV1Input {
                kind: wire::FileKindV1 {
                    value: match kind {
                        FileKind::Regular => 0,
                        FileKind::Executable => 1,
                        FileKind::Symlink => unreachable!(),
                    },
                },
                before: content_id(*before),
                expected: content_id(*expected),
                replacement: content_id(*replacement),
                after: content_id(*after),
                start_anchor: *start_anchor,
                end_anchor: *end_anchor,
            }
            .into(),
        ),
        EditOperation::ExactFile {
            expected,
            replacement,
        } => wire::EditOperationV1::ExactFileEditV1(
            wire::ExactFileEditV1Input {
                expected: expected.iter().copied().map(file_value).collect(),
                replacement: replacement.iter().copied().map(file_value).collect(),
            }
            .into(),
        ),
    };
    encode(wire::WheatObject::EditV1(
        wire::EditV1Input {
            path: edit.path.clone(),
            operation,
        }
        .into(),
    ))
}

fn validate_path(path: &str) -> Result<(), CanonicalObjectError> {
    if path.is_empty()
        || path
            .split('/')
            .any(|part| validate_directory_component(part).is_err())
    {
        return Err(CanonicalObjectError::InvalidDirectoryComponent(
            path.to_owned(),
        ));
    }
    Ok(())
}

fn guarded_file_kind(value: u8) -> Result<FileKind, CanonicalObjectError> {
    match value {
        0 => Ok(FileKind::Regular),
        1 => Ok(FileKind::Executable),
        _ => Err(CanonicalObjectError::Decode(
            "invalid guarded text file kind".into(),
        )),
    }
}

fn validate_object(object: &wire::WheatObject) -> Result<(), CanonicalObjectError> {
    match object {
        wire::WheatObject::EditV1(edit) => {
            validate_path(&edit.path)?;
            match &edit.operation {
                wire::EditOperationV1::GuardedTextEditV1(value) => {
                    guarded_file_kind(value.kind.value)?;
                    validate_content_id(&value.before)?;
                    validate_content_id(&value.expected)?;
                    validate_content_id(&value.replacement)?;
                    validate_content_id(&value.after)?;
                    if value.expected == value.replacement {
                        return Err(CanonicalObjectError::InvalidCollection(
                            "guarded text edit must not be a no-op",
                        ));
                    }
                }
                wire::EditOperationV1::ExactFileEditV1(value) => {
                    if value.expected.len() > 1 || value.replacement.len() > 1 {
                        return Err(CanonicalObjectError::InvalidCollection(
                            "exact file option cardinality",
                        ));
                    }
                    value.expected.first().map_or(Ok(()), validate_file_value)?;
                    value
                        .replacement
                        .first()
                        .map_or(Ok(()), validate_file_value)?;
                    if value.expected == value.replacement {
                        return Err(CanonicalObjectError::InvalidCollection(
                            "exact file edit must change value",
                        ));
                    }
                }
            }
        }
        wire::WheatObject::TreeV1(tree) => {
            validate_sorted_unique_by(
                &tree.entries,
                |entry| entry.name.as_str(),
                "directory entries",
            )?;
            for entry in &tree.entries {
                validate_directory_component(&entry.name)?;
                if let wire::DirectoryEntryValueV1::FileEntryV1(file) = &entry.value {
                    validate_file_value(&file.value)?;
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
                &revision.combines,
                |input| input.digest.bytes.as_slice(),
                "revision combines",
            )?;
            if revision.combines.len() > crate::MAX_COMBINES {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision combines too many revisions",
                ));
            }
            match revision.predecessors.first() {
                None if !revision.combines.is_empty() => {
                    return Err(CanonicalObjectError::InvalidCollection(
                        "combined revision must have a predecessor",
                    ));
                }
                Some(predecessor) if revision.combines.contains(predecessor) => {
                    return Err(CanonicalObjectError::InvalidCollection(
                        "combined revision must not combine its predecessor",
                    ));
                }
                _ => {}
            }
            if (revision.generation == 0) != revision.predecessors.is_empty() {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision generation must be 0 exactly when it has no predecessor",
                ));
            }
            validate_sorted_unique_by(
                &revision.dependencies,
                |dependency| dependency.uuid.bytes.as_slice(),
                "revision dependencies",
            )?;
            validate_edit_ids(&revision.edits, "revision edits")?;
            validate_transition(&revision.transition, &revision.edits)?;
        }
        wire::WheatObject::ApplicationV1(application) => {
            // The recurrence is checked against the applications named; the shape
            // alone is checked here, so a walk of the index never indexes past its end.
            let position = application.position;
            if application.previous.len() > 1
                || application.skip.len() != crate::ApplicationOrder::skip_levels(position)
                || application.window_max.len() != crate::ApplicationOrder::window_levels(position)
                || application.previous.first() != application.skip.first()
            {
                return Err(CanonicalObjectError::InvalidCollection(
                    "application order index does not fit its position",
                ));
            }
            validate_sorted_unique_by(
                &application.applied,
                |e| e.edit.digest.bytes.as_slice(),
                "application applied edits",
            )?;
            for entry in &application.applied {
                validate_applied_evidence(&entry.evidence)?;
            }
        }
        wire::WheatObject::ConflictV1(conflict) => {
            validate_sorted_unique_by(
                &conflict.edits,
                |e| e.edit.digest.bytes.as_slice(),
                "conflict edits",
            )?;
            for entry in &conflict.edits {
                validate_failed_evidence(&entry.evidence)?;
            }
        }
        wire::WheatObject::ViewV1(view) => {
            if view.last.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "view must have at most one last change",
                ));
            }
            // The base has no changes; any other view has a last change and at least
            // weight 1 for each of its changes.
            let consistent = if view.size == 0 {
                view.last.is_empty() && view.weight == 0
            } else {
                !view.last.is_empty() && view.weight >= view.size
            };
            if !consistent {
                return Err(CanonicalObjectError::InvalidCollection(
                    "view size, last change and weight disagree",
                ));
            }
        }
        wire::WheatObject::ContentMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "content map must have at most one value",
                ));
            }
            if node.value.contains(&0) || node.entries.iter().any(|entry| entry.value == 0) {
                return Err(CanonicalObjectError::InvalidCollection(
                    "content map counts are at least 1",
                ));
            }
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                32,
                "content map node",
            )?;
        }
        wire::WheatObject::RevisionMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "revision map must have at most one value",
                ));
            }
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                16,
                "revision map node",
            )?;
        }
        wire::WheatObject::ChangeMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "change map must have at most one value",
                ));
            }
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                16,
                "change map node",
            )?;
        }
        wire::WheatObject::ApplicationMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "application map value",
                ));
            }
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                16,
                "application map node",
            )?;
        }
        wire::WheatObject::ConflictSetNodeV1(node) => {
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                32,
                "conflict set node",
            )?;
        }
        wire::WheatObject::ContentSetNodeV1(node) => {
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                32,
                "content set node",
            )?;
        }
        wire::WheatObject::RetainedSetNodeV1(node) => {
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                32,
                "retained set node",
            )?;
        }
        wire::WheatObject::ViewLineageMapNodeV1(node) => {
            if node.value.len() > 1 {
                return Err(CanonicalObjectError::InvalidCollection(
                    "view lineage map value cardinality",
                ));
            }
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                !node.value.is_empty(),
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                32,
                "view lineage map node",
            )?;
        }
        wire::WheatObject::ProjectedCommitSetNodeV1(node) => {
            let efb: Vec<u8> = node
                .entries
                .iter()
                .filter_map(|e| e.suffix.first().copied())
                .collect();
            validate_radix_node(
                &node.prefix,
                node.present,
                &node.children.iter().map(|c| c.edge).collect::<Vec<_>>(),
                &efb,
                crate::ProjectedCommit::LEN,
                "projected commit set node",
            )?;
        }
        wire::WheatObject::RepositoryStateV1(_) | wire::WheatObject::RepositoryEnvelopeV1(_) => {}
        wire::WheatObject::ViewLabelManifestV1(manifest) => {
            validate_sorted_unique_by(
                &manifest.labels,
                |entry| entry.name.as_str(),
                "view label manifest entries",
            )?;
            for entry in &manifest.labels {
                crate::branch_records::validate_label_name(&entry.name)?;
                if entry.original_git_refs.len() > 1 {
                    return Err(CanonicalObjectError::InvalidCollection(
                        "view label original Git ref cardinality",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn object_kind(object: &wire::WheatObject) -> CanonicalObjectKind {
    match object {
        wire::WheatObject::TreeV1(_) => CanonicalObjectKind::Tree,
        wire::WheatObject::EditV1(_) => CanonicalObjectKind::Edit,
        wire::WheatObject::ChangeV1(_) => CanonicalObjectKind::Change,
        wire::WheatObject::RevisionV1(_) => CanonicalObjectKind::Revision,
        wire::WheatObject::ApplicationV1(_) => CanonicalObjectKind::Application,
        wire::WheatObject::ConflictV1(_) => CanonicalObjectKind::Conflict,
        wire::WheatObject::ViewV1(_) => CanonicalObjectKind::View,
        wire::WheatObject::ContentMapNodeV1(_) => CanonicalObjectKind::ContentMapNode,
        wire::WheatObject::RepositoryStateV1(_) => CanonicalObjectKind::RepositoryState,
        wire::WheatObject::ViewLabelManifestV1(_) => CanonicalObjectKind::ViewLabelManifest,
        wire::WheatObject::RepositoryEnvelopeV1(_) => CanonicalObjectKind::RepositoryEnvelope,
        wire::WheatObject::RevisionMapNodeV1(_) => CanonicalObjectKind::RevisionMapNode,
        wire::WheatObject::ChangeMapNodeV1(_) => CanonicalObjectKind::ChangeMapNode,
        wire::WheatObject::ApplicationMapNodeV1(_) => CanonicalObjectKind::ApplicationMapNode,
        wire::WheatObject::ConflictSetNodeV1(_) => CanonicalObjectKind::ConflictSetNode,
        wire::WheatObject::ContentSetNodeV1(_) => CanonicalObjectKind::ContentSetNode,
        wire::WheatObject::RetainedSetNodeV1(_) => CanonicalObjectKind::RetainedSetNode,
        wire::WheatObject::ViewLineageMapNodeV1(_) => CanonicalObjectKind::ViewLineageMapNode,
        wire::WheatObject::ProjectedCommitSetNodeV1(_) => {
            CanonicalObjectKind::ProjectedCommitSetNode
        }
    }
}

fn validate_edit_ids(
    values: &[wire::EditId],
    name: &'static str,
) -> Result<(), CanonicalObjectError> {
    validate_sorted_unique_by(values, |id| id.digest.bytes.as_slice(), name)
}

fn validate_transition(
    value: &wire::EditTransitionV1,
    selected: &[wire::EditId],
) -> Result<(), CanonicalObjectError> {
    validate_edit_ids(&value.preserved, "preserved edits")?;
    validate_edit_ids(&value.added, "added edits")?;
    validate_edit_ids(&value.removed, "removed edits")?;
    let selected_set: BTreeSet<_> = selected
        .iter()
        .map(|id| id.digest.bytes.as_slice())
        .collect();
    let preserved: BTreeSet<_> = value
        .preserved
        .iter()
        .map(|id| id.digest.bytes.as_slice())
        .collect();
    let added: BTreeSet<_> = value
        .added
        .iter()
        .map(|id| id.digest.bytes.as_slice())
        .collect();
    let removed: BTreeSet<_> = value
        .removed
        .iter()
        .map(|id| id.digest.bytes.as_slice())
        .collect();
    if !preserved.is_disjoint(&added)
        || !preserved.is_disjoint(&removed)
        || !added.is_disjoint(&removed)
        || selected_set != preserved.union(&added).copied().collect()
    {
        return Err(CanonicalObjectError::InvalidCollection(
            "invalid edit transition partition",
        ));
    }
    validate_sorted_unique_by(
        &value.subsumed,
        |s| s.source.digest.bytes.as_slice(),
        "edit subsumptions",
    )?;
    for s in &value.subsumed {
        validate_edit_ids(&s.replacements, "subsumption replacements")?;
        if s.replacements.is_empty()
            || !removed.contains(s.source.digest.bytes.as_slice())
            || !s
                .replacements
                .iter()
                .all(|id| added.contains(id.digest.bytes.as_slice()))
        {
            return Err(CanonicalObjectError::InvalidCollection(
                "invalid edit subsumption",
            ));
        }
    }
    validate_sorted_unique_by(
        &value.resolutions,
        |r| r.application.digest.bytes.as_slice(),
        "conflict resolutions",
    )?;
    for r in &value.resolutions {
        validate_edit_ids(&r.conflicted_edits, "resolution conflicted edits")?;
        validate_edit_ids(&r.replacement_edits, "resolution replacement edits")?;
        if r.conflicted_edits.is_empty()
            || !r
                .replacement_edits
                .iter()
                .all(|id| selected_set.contains(id.digest.bytes.as_slice()))
        {
            return Err(CanonicalObjectError::InvalidCollection(
                "invalid conflict resolution evidence",
            ));
        }
    }
    Ok(())
}

fn validate_range(range: &wire::ByteRangeV1) -> Result<(), CanonicalObjectError> {
    if range.start > range.end {
        Err(CanonicalObjectError::InvalidCollection(
            "invalid byte range",
        ))
    } else {
        Ok(())
    }
}
fn validate_applied_evidence(
    value: &wire::AppliedEditEvidenceV1,
) -> Result<(), CanonicalObjectError> {
    if value.target.len() > 1 || value.read.len() > 1 || value.write.len() > 1 {
        return Err(CanonicalObjectError::InvalidCollection(
            "edit evidence option cardinality",
        ));
    }
    value.target.first().map_or(Ok(()), validate_file_value)?;
    if let Some(read) = value.read.first() {
        validate_range(read)?;
    }
    if let Some(write) = value.write.first() {
        validate_range(write)?;
    }
    if let Some(write) = value.write.first() {
        let Some(read) = value.read.first() else {
            return Err(CanonicalObjectError::InvalidCollection(
                "write range requires read range",
            ));
        };
        if write.start < read.start || write.end > read.end {
            return Err(CanonicalObjectError::InvalidCollection(
                "write range must be contained in read range",
            ));
        }
    }
    Ok(())
}
fn validate_failed_evidence(
    value: &wire::FailedEditEvidenceV1,
) -> Result<(), CanonicalObjectError> {
    if value.target.len() > 1 {
        return Err(CanonicalObjectError::InvalidCollection(
            "failed edit target option cardinality",
        ));
    }
    value.target.first().map_or(Ok(()), validate_file_value)?;
    if let wire::EditFailureV1::InterferenceFailureV1(v) = &value.reason {
        validate_edit_ids(&v.with, "interfering edits")?;
    }
    Ok(())
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

fn validate_file_value(value: &wire::FileValueV1) -> Result<(), CanonicalObjectError> {
    validate_content_id(&value.content)?;
    if value.kind.value > 2 {
        return Err(CanonicalObjectError::Decode("invalid file kind".into()));
    }
    Ok(())
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

fn change_id(value: ChangeId) -> wire::ChangeId {
    wire::ChangeId {
        uuid: wire::Uuid128 {
            bytes: value.as_bytes().to_vec(),
        },
    }
}

fn file_value_domain(value: &wire::FileValueV1) -> Result<FileValue, CanonicalObjectError> {
    let kind = match value.kind.value {
        0 => FileKind::Regular,
        1 => FileKind::Executable,
        2 => FileKind::Symlink,
        _ => return Err(CanonicalObjectError::Decode("invalid file kind".into())),
    };
    Ok(FileValue {
        content: content_id_domain(&value.content)?,
        kind,
    })
}

fn file_value(value: FileValue) -> wire::FileValueV1 {
    wire::FileValueV1 {
        content: content_id(value.content),
        kind: wire::FileKindV1 {
            value: match value.kind {
                FileKind::Regular => 0,
                FileKind::Executable => 1,
                FileKind::Symlink => 2,
            },
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
wire_digest_id!(edit_id, crate::EditId, EditId);
wire_digest_id!(revision_map_id, crate::RevisionMapId, RevisionMapId);
wire_digest_id!(change_map_id, crate::ChangeMapId, ChangeMapId);
wire_digest_id!(change_object_id, crate::ChangeObjectId, ChangeObjectId);
wire_digest_id!(
    application_map_id,
    crate::ApplicationMapId,
    ApplicationMapId
);
wire_digest_id!(conflict_set_id, crate::ConflictSetId, ConflictSetId);
wire_digest_id!(content_set_id, ContentSetId, ContentSetId);
wire_digest_id!(retained_set_id, crate::RetainedSetId, RetainedSetId);
wire_digest_id!(
    projected_commit_set_id,
    crate::ProjectedCommitSetId,
    ProjectedCommitSetId
);
wire_digest_id!(
    view_lineage_map_id,
    crate::ViewLineageMapId,
    ViewLineageMapId
);
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
    view_label_manifest_id,
    crate::ViewLabelManifestId,
    ViewLabelManifestId
);
wire_digest_id!(content_map_id, ContentMapId, ContentMapId);

fn digest(bytes: &[u8; 32]) -> wire::Digest256 {
    wire::Digest256 {
        bytes: bytes.to_vec(),
    }
}

fn encode(object: wire::WheatObject) -> Vec<u8> {
    let bytes = object
        .encode()
        .expect("validated Wheat domain values must encode");
    if let Err(error) = validate_canonical_object(&bytes) {
        panic!("validated Wheat domain values must encode canonically: {error:?}");
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotation(message: &str) -> crate::LabelAnnotation {
        crate::LabelAnnotation {
            tagger: Actor {
                name: "Bart".into(),
                email: "bart@example.com".into(),
            },
            tagged_at: Timestamp {
                seconds: 1_790_000_000,
                offset_minutes: 540,
            },
            message: message.into(),
        }
    }

    #[test]
    fn label_annotations_round_trip_and_change_the_manifest_id() {
        let view = crate::ViewId::from_digest([7u8; 32]);
        let plain = crate::ViewLabel::from(view);
        let tagged = crate::ViewLabel {
            annotation: Some(annotation("Release 0.1.0\n\nFirst release.")),
            ..plain.clone()
        };
        let record = crate::ViewLabelManifestRecord::new([
            ("main", plain.clone()),
            ("tags/v0.1.0", tagged.clone()),
        ])
        .unwrap();
        let decoded = decode_view_label_manifest_record(&record.canonical_bytes()).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.labels["tags/v0.1.0"], tagged);
        assert_eq!(decoded.labels["main"].annotation, None);

        let untagged =
            crate::ViewLabelManifestRecord::new([("main", plain.clone()), ("tags/v0.1.0", plain)])
                .unwrap();
        assert_ne!(untagged.id(), record.id());
    }

    #[test]
    fn label_annotations_must_fit_a_git_tag() {
        let view = crate::ViewId::from_digest([7u8; 32]);
        let label = |annotation: crate::LabelAnnotation| crate::ViewLabel {
            view,
            original_git_ref: None,
            annotation: Some(annotation),
        };
        let mut bad_tagger = annotation("Release");
        bad_tagger.tagger.email = "<bart@example.com>".into();
        let mut empty_name = annotation("Release");
        empty_name.tagger.name.clear();
        for invalid in [
            annotation(""),
            annotation(" \n"),
            annotation("nul\0inside"),
            annotation(&"x".repeat(crate::MAX_LABEL_ANNOTATION_MESSAGE_BYTES + 1)),
            bad_tagger,
            empty_name,
        ] {
            assert!(matches!(
                crate::ViewLabelManifestRecord::new([("tags/v1", label(invalid))]),
                Err(CanonicalObjectError::InvalidLabelAnnotation(_))
            ));
        }
        crate::ViewLabelManifestRecord::new([(
            "tags/v1",
            label(annotation(
                &"x".repeat(crate::MAX_LABEL_ANNOTATION_MESSAGE_BYTES),
            )),
        )])
        .unwrap();
    }
}
