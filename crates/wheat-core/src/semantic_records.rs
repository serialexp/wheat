//! Semantic representations of persisted objects which refer to other objects by ID.
//!
//! These records are deliberately distinct from the materialized model types: decoding a
//! revision or application must not require loading the trees and conflicts it references.

use std::collections::{BTreeMap, BTreeSet};

use wheat_binschema::generated as binschema;

use crate::{
    Actor, ApplicationId, ApplicationOrder, AppliedEditEvidence, ByteRange, CanonicalObjectError,
    CanonicalObjectKind, Change, ChangeId, ChangeMetadata, Conflict, ConflictId,
    ConflictResolutionEvidence, ContentId, EditFailure, EditId, EditSubsumption, EditTransition,
    FailedEditEvidence, FileKind, FileValue, RevisionId, RevisionMetadata, SourceProvenance,
    Timestamp, TreeId, ViewId, validate_canonical_object,
};

const BLAKE3_256: u8 = 1;

/// The persisted, non-materialized representation of a change revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionRecord {
    pub change_id: ChangeId,
    pub metadata: RevisionMetadata,
    pub predecessor: Option<RevisionId>,
    /// A combined revision's inputs other than its predecessor.
    pub combines: BTreeSet<RevisionId>,
    pub generation: u64,
    pub dependencies: BTreeSet<ChangeId>,
    pub authored_against: ViewId,
    pub edits: BTreeSet<EditId>,
    pub transition: EditTransition,
    pub resulting_tree: TreeId,
}

impl RevisionRecord {
    /// Encode this record in the canonical Wheat object format.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.transition
            .validate(&self.edits)
            .expect("revision record must contain a valid edit transition");
        encode(binschema::WheatObject::RevisionV1(
            binschema::RevisionV1Input {
                change_id: change_id_wire(self.change_id),
                metadata: revision_metadata_wire(&self.metadata),
                predecessors: self.predecessor.into_iter().map(revision_id_wire).collect(),
                combines: self
                    .combines
                    .iter()
                    .copied()
                    .map(revision_id_wire)
                    .collect(),
                generation: self.generation,
                dependencies: self
                    .dependencies
                    .iter()
                    .copied()
                    .map(change_id_wire)
                    .collect(),
                authored_against: view_id_wire(self.authored_against),
                edits: self.edits.iter().copied().map(edit_id_wire).collect(),
                transition: edit_transition_wire(&self.transition),
                resulting_tree: tree_id_wire(self.resulting_tree),
            }
            .into(),
        ))
    }

    /// Return the content-addressed ID of this revision record.
    #[must_use]
    pub fn id(&self) -> RevisionId {
        RevisionId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// The persisted result of applying a revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationResultRecord {
    Clean {
        tree: TreeId,
    },
    Conflicted {
        partial_tree: TreeId,
        conflict: ConflictId,
    },
}

/// The persisted, non-materialized representation of an application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRecord {
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub order: ApplicationOrder,
    pub result: ApplicationResultRecord,
    pub applied: BTreeMap<EditId, AppliedEditEvidence>,
}

impl ApplicationRecord {
    /// Encode this record in the canonical Wheat object format.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let result = match &self.result {
            ApplicationResultRecord::Clean { tree } => {
                binschema::ApplicationResultV1::CleanApplicationV1(
                    binschema::CleanApplicationV1Input {
                        resulting_tree: tree_id_wire(*tree),
                    }
                    .into(),
                )
            }
            ApplicationResultRecord::Conflicted {
                partial_tree,
                conflict,
            } => binschema::ApplicationResultV1::ConflictedApplicationV1(
                binschema::ConflictedApplicationV1Input {
                    partial_tree: tree_id_wire(*partial_tree),
                    conflict: conflict_id_wire(*conflict),
                }
                .into(),
            ),
        };
        encode(binschema::WheatObject::ApplicationV1(
            binschema::ApplicationV1Input {
                revision_id: revision_id_wire(self.revision_id),
                target_view: view_id_wire(self.target_view),
                position: self.order.position,
                previous: self
                    .order
                    .previous
                    .into_iter()
                    .map(application_id_wire)
                    .collect(),
                skip: self
                    .order
                    .skip
                    .iter()
                    .copied()
                    .map(application_id_wire)
                    .collect(),
                window_max: self
                    .order
                    .window_max
                    .iter()
                    .copied()
                    .map(change_id_wire)
                    .collect(),
                result,
                applied: self
                    .applied
                    .iter()
                    .map(|(edit, evidence)| binschema::AppliedEditEntryV1 {
                        edit: edit_id_wire(*edit),
                        evidence: applied_evidence_wire(evidence),
                    })
                    .collect(),
            }
            .into(),
        ))
    }

    /// Return the content-addressed ID of this application record.
    #[must_use]
    pub fn id(&self) -> ApplicationId {
        ApplicationId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// Decode and semantically convert a canonical revision object.
pub fn decode_revision_record(bytes: &[u8]) -> Result<RevisionRecord, CanonicalObjectError> {
    require_kind(bytes, CanonicalObjectKind::Revision)?;
    let revision = binschema::RevisionV1::decode(bytes).map_err(decode_error)?;

    Ok(RevisionRecord {
        change_id: change_id_domain(&revision.change_id)?,
        metadata: revision_metadata_domain(revision.metadata),
        predecessor: revision
            .predecessors
            .first()
            .map(revision_id_domain)
            .transpose()?,
        combines: revision
            .combines
            .iter()
            .map(revision_id_domain)
            .collect::<Result<_, _>>()?,
        generation: revision.generation,
        dependencies: revision
            .dependencies
            .iter()
            .map(change_id_domain)
            .collect::<Result<_, _>>()?,
        authored_against: view_id_domain(&revision.authored_against)?,
        edits: revision
            .edits
            .iter()
            .map(edit_id_domain)
            .collect::<Result<_, _>>()?,
        transition: edit_transition_domain(revision.transition)?,
        resulting_tree: tree_id_domain(&revision.resulting_tree)?,
    })
}

/// Decode and semantically convert a canonical application object.
pub fn decode_application_record(bytes: &[u8]) -> Result<ApplicationRecord, CanonicalObjectError> {
    require_kind(bytes, CanonicalObjectKind::Application)?;
    let application = binschema::ApplicationV1::decode(bytes).map_err(decode_error)?;
    let result = match application.result {
        binschema::ApplicationResultV1::CleanApplicationV1(clean) => {
            ApplicationResultRecord::Clean {
                tree: tree_id_domain(&clean.resulting_tree)?,
            }
        }
        binschema::ApplicationResultV1::ConflictedApplicationV1(conflicted) => {
            ApplicationResultRecord::Conflicted {
                partial_tree: tree_id_domain(&conflicted.partial_tree)?,
                conflict: conflict_id_domain(&conflicted.conflict)?,
            }
        }
    };

    Ok(ApplicationRecord {
        revision_id: revision_id_domain(&application.revision_id)?,
        target_view: view_id_domain(&application.target_view)?,
        order: ApplicationOrder {
            position: application.position,
            previous: application
                .previous
                .first()
                .map(application_id_domain)
                .transpose()?,
            skip: application
                .skip
                .iter()
                .map(application_id_domain)
                .collect::<Result<_, _>>()?,
            window_max: application
                .window_max
                .iter()
                .map(change_id_domain)
                .collect::<Result<_, _>>()?,
        },
        result,
        applied: application
            .applied
            .into_iter()
            .map(|entry| {
                Ok((
                    edit_id_domain(&entry.edit)?,
                    applied_evidence_domain(entry.evidence)?,
                ))
            })
            .collect::<Result<_, CanonicalObjectError>>()?,
    })
}

/// Decode and semantically convert a canonical change object.
pub fn decode_change(bytes: &[u8]) -> Result<Change, CanonicalObjectError> {
    require_kind(bytes, CanonicalObjectKind::Change)?;
    let change = binschema::ChangeV1::decode(bytes).map_err(decode_error)?;
    Ok(Change {
        id: change_id_domain(&change.change_id)?,
        metadata: change_metadata_domain(change.metadata),
    })
}

/// Decode and semantically convert a canonical conflict object.
///
/// A conflict's ID is derived from the canonical bytes rather than represented in its payload.
pub fn decode_conflict(bytes: &[u8]) -> Result<Conflict, CanonicalObjectError> {
    require_kind(bytes, CanonicalObjectKind::Conflict)?;
    let conflict = binschema::ConflictV1::decode(bytes).map_err(decode_error)?;
    let edits = conflict
        .edits
        .into_iter()
        .map(|entry| {
            Ok((
                edit_id_domain(&entry.edit)?,
                failed_evidence_domain(entry.evidence)?,
            ))
        })
        .collect::<Result<_, CanonicalObjectError>>()?;
    Ok(Conflict {
        id: ConflictId::of_canonical_bytes(bytes),
        base_view: view_id_domain(&conflict.target_view)?,
        edits,
    })
}

fn require_kind(bytes: &[u8], expected: CanonicalObjectKind) -> Result<(), CanonicalObjectError> {
    let actual = validate_canonical_object(bytes)?;
    if actual != expected {
        return Err(CanonicalObjectError::WrongKind { expected, actual });
    }
    Ok(())
}

fn decode_error(error: impl std::fmt::Debug) -> CanonicalObjectError {
    CanonicalObjectError::Decode(format!("{error:?}"))
}

fn actor_domain(actor: binschema::ActorV1) -> Actor {
    Actor {
        name: actor.name,
        email: actor.email,
    }
}

fn timestamp_domain(timestamp: binschema::TimestampV1) -> Timestamp {
    Timestamp {
        seconds: timestamp.seconds,
        offset_minutes: timestamp.offset_minutes,
    }
}

fn provenance_domain(provenance: binschema::SourceProvenanceV1) -> SourceProvenance {
    SourceProvenance {
        system: provenance.system,
        object_id: provenance.object_id,
        parent_object_ids: provenance
            .parent_object_ids
            .into_iter()
            .map(|id| id.bytes)
            .collect(),
    }
}

fn change_metadata_domain(metadata: binschema::ChangeMetadataV1) -> ChangeMetadata {
    ChangeMetadata {
        title: metadata.title,
        description: metadata.description,
        creator: actor_domain(metadata.creator),
        created_at: timestamp_domain(metadata.created_at),
        provenance: metadata.provenance.map(provenance_domain),
    }
}

fn revision_metadata_domain(metadata: binschema::RevisionMetadataV1) -> RevisionMetadata {
    RevisionMetadata {
        message: metadata.message,
        author: actor_domain(metadata.author),
        authored_at: timestamp_domain(metadata.authored_at),
        committer: actor_domain(metadata.committer),
        committed_at: timestamp_domain(metadata.committed_at),
        provenance: metadata.provenance.map(provenance_domain),
    }
}

fn edit_transition_domain(
    value: binschema::EditTransitionV1,
) -> Result<EditTransition, CanonicalObjectError> {
    Ok(EditTransition {
        preserved: value
            .preserved
            .iter()
            .map(edit_id_domain)
            .collect::<Result<_, _>>()?,
        added: value
            .added
            .iter()
            .map(edit_id_domain)
            .collect::<Result<_, _>>()?,
        removed: value
            .removed
            .iter()
            .map(edit_id_domain)
            .collect::<Result<_, _>>()?,
        subsumed: value
            .subsumed
            .into_iter()
            .map(|s| {
                Ok(EditSubsumption {
                    source: edit_id_domain(&s.source)?,
                    replacements: s
                        .replacements
                        .iter()
                        .map(edit_id_domain)
                        .collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<_, CanonicalObjectError>>()?,
        resolutions: value
            .resolutions
            .into_iter()
            .map(|r| {
                Ok(ConflictResolutionEvidence {
                    application: application_id_domain(&r.application)?,
                    conflicted_edits: r
                        .conflicted_edits
                        .iter()
                        .map(edit_id_domain)
                        .collect::<Result<_, _>>()?,
                    replacement_edits: r
                        .replacement_edits
                        .iter()
                        .map(edit_id_domain)
                        .collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<_, CanonicalObjectError>>()?,
    })
}

fn range_domain(value: binschema::ByteRangeV1) -> ByteRange {
    ByteRange {
        start: value.start,
        end: value.end,
    }
}
fn applied_evidence_domain(
    value: binschema::AppliedEditEvidenceV1,
) -> Result<AppliedEditEvidence, CanonicalObjectError> {
    Ok(AppliedEditEvidence {
        target: value.target.first().map(file_value_domain).transpose()?,
        read: value.read.into_iter().next().map(range_domain),
        write: value.write.into_iter().next().map(range_domain),
    })
}
fn failed_evidence_domain(
    value: binschema::FailedEditEvidenceV1,
) -> Result<FailedEditEvidence, CanonicalObjectError> {
    let reason = match value.reason {
        binschema::EditFailureV1::NoMatchFailureV1(_) => EditFailure::NoMatch,
        binschema::EditFailureV1::AmbiguousFailureV1(_) => EditFailure::Ambiguous,
        binschema::EditFailureV1::InterferenceFailureV1(v) => EditFailure::Interference {
            with: v
                .with
                .iter()
                .map(edit_id_domain)
                .collect::<Result<_, _>>()?,
        },
        binschema::EditFailureV1::KindMismatchFailureV1(_) => EditFailure::KindMismatch,
        binschema::EditFailureV1::ExpectedFileMismatchFailureV1(_) => {
            EditFailure::ExpectedFileMismatch
        }
    };
    Ok(FailedEditEvidence {
        target: value.target.first().map(file_value_domain).transpose()?,
        reason,
    })
}

pub(crate) fn change_id_domain(id: &binschema::ChangeId) -> Result<ChangeId, CanonicalObjectError> {
    let bytes = id
        .uuid
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| CanonicalObjectError::Decode("change UUID is not 16 bytes".into()))?;
    Ok(ChangeId::from_bytes(bytes))
}

fn file_value_domain(value: &binschema::FileValueV1) -> Result<FileValue, CanonicalObjectError> {
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

fn content_id_domain(id: &binschema::ContentId) -> Result<ContentId, CanonicalObjectError> {
    if id.algorithm != BLAKE3_256 {
        return Err(CanonicalObjectError::UnsupportedContentAlgorithm(
            id.algorithm,
        ));
    }
    Ok(ContentId::from_digest(digest_domain(&id.digest)?))
}

fn digest_domain(digest: &binschema::Digest256) -> Result<[u8; 32], CanonicalObjectError> {
    digest
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| CanonicalObjectError::Decode("digest is not 32 bytes".into()))
}

macro_rules! domain_digest_id {
    ($function:ident, $wire:ty, $domain:ty) => {
        fn $function(id: &$wire) -> Result<$domain, CanonicalObjectError> {
            Ok(<$domain>::from_digest(digest_domain(&id.digest)?))
        }
    };
}

domain_digest_id!(revision_id_domain, binschema::RevisionId, RevisionId);
domain_digest_id!(edit_id_domain, binschema::EditId, EditId);
domain_digest_id!(
    application_id_domain,
    binschema::ApplicationId,
    ApplicationId
);
domain_digest_id!(tree_id_domain, binschema::TreeId, TreeId);
domain_digest_id!(conflict_id_domain, binschema::ConflictId, ConflictId);
domain_digest_id!(view_id_domain, binschema::ViewId, ViewId);

fn actor_wire(actor: &Actor) -> binschema::ActorV1 {
    binschema::ActorV1 {
        name: actor.name.clone(),
        email: actor.email.clone(),
    }
}

fn timestamp_wire(timestamp: Timestamp) -> binschema::TimestampV1 {
    binschema::TimestampV1 {
        seconds: timestamp.seconds,
        offset_minutes: timestamp.offset_minutes,
    }
}

fn provenance_wire(provenance: &SourceProvenance) -> binschema::SourceProvenanceV1 {
    binschema::SourceProvenanceV1 {
        system: provenance.system.clone(),
        object_id: provenance.object_id.clone(),
        parent_object_ids: provenance
            .parent_object_ids
            .iter()
            .cloned()
            .map(|bytes| binschema::SourceObjectIdV1 { bytes })
            .collect(),
    }
}

fn revision_metadata_wire(metadata: &RevisionMetadata) -> binschema::RevisionMetadataV1 {
    binschema::RevisionMetadataV1 {
        message: metadata.message.clone(),
        author: actor_wire(&metadata.author),
        authored_at: timestamp_wire(metadata.authored_at),
        committer: actor_wire(&metadata.committer),
        committed_at: timestamp_wire(metadata.committed_at),
        provenance: metadata.provenance.as_ref().map(provenance_wire),
    }
}

fn edit_transition_wire(value: &EditTransition) -> binschema::EditTransitionV1 {
    let mut subsumed = value
        .subsumed
        .iter()
        .map(|s| binschema::EditSubsumptionV1 {
            source: edit_id_wire(s.source),
            replacements: s.replacements.iter().copied().map(edit_id_wire).collect(),
        })
        .collect::<Vec<_>>();
    subsumed.sort_by(|a, b| a.source.digest.bytes.cmp(&b.source.digest.bytes));
    let mut resolutions = value
        .resolutions
        .iter()
        .map(|r| binschema::ConflictResolutionEvidenceV1 {
            application: application_id_wire(r.application),
            conflicted_edits: r
                .conflicted_edits
                .iter()
                .copied()
                .map(edit_id_wire)
                .collect(),
            replacement_edits: r
                .replacement_edits
                .iter()
                .copied()
                .map(edit_id_wire)
                .collect(),
        })
        .collect::<Vec<_>>();
    resolutions.sort_by(|a, b| a.application.digest.bytes.cmp(&b.application.digest.bytes));
    binschema::EditTransitionV1 {
        preserved: value.preserved.iter().copied().map(edit_id_wire).collect(),
        added: value.added.iter().copied().map(edit_id_wire).collect(),
        removed: value.removed.iter().copied().map(edit_id_wire).collect(),
        subsumed,
        resolutions,
    }
}
fn range_wire(value: ByteRange) -> binschema::ByteRangeV1 {
    binschema::ByteRangeV1 {
        start: value.start,
        end: value.end,
    }
}
fn applied_evidence_wire(value: &AppliedEditEvidence) -> binschema::AppliedEditEvidenceV1 {
    binschema::AppliedEditEvidenceV1 {
        target: value.target.map(file_value_wire).into_iter().collect(),
        read: value.read.map(range_wire).into_iter().collect(),
        write: value.write.map(range_wire).into_iter().collect(),
    }
}

fn change_id_wire(id: ChangeId) -> binschema::ChangeId {
    binschema::ChangeId {
        uuid: binschema::Uuid128 {
            bytes: id.as_bytes().to_vec(),
        },
    }
}

fn file_value_wire(value: FileValue) -> binschema::FileValueV1 {
    binschema::FileValueV1 {
        content: content_id_wire(value.content),
        kind: binschema::FileKindV1 {
            value: match value.kind {
                FileKind::Regular => 0,
                FileKind::Executable => 1,
                FileKind::Symlink => 2,
            },
        },
    }
}

fn content_id_wire(id: ContentId) -> binschema::ContentId {
    binschema::ContentId {
        algorithm: BLAKE3_256,
        digest: digest_wire(id.as_bytes()),
    }
}

macro_rules! wire_digest_id {
    ($function:ident, $domain:ty, $wire:ident) => {
        fn $function(id: $domain) -> binschema::$wire {
            binschema::$wire {
                digest: digest_wire(id.as_bytes()),
            }
        }
    };
}

wire_digest_id!(revision_id_wire, RevisionId, RevisionId);
wire_digest_id!(edit_id_wire, EditId, EditId);
wire_digest_id!(application_id_wire, ApplicationId, ApplicationId);
wire_digest_id!(tree_id_wire, TreeId, TreeId);
wire_digest_id!(conflict_id_wire, ConflictId, ConflictId);
wire_digest_id!(view_id_wire, ViewId, ViewId);

fn digest_wire(bytes: &[u8; 32]) -> binschema::Digest256 {
    binschema::Digest256 {
        bytes: bytes.to_vec(),
    }
}

fn encode(object: binschema::WheatObject) -> Vec<u8> {
    object
        .encode()
        .expect("validated Wheat semantic records must encode")
}
