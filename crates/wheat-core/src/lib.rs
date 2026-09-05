//! Git-independent source-control primitives for Wheat.
//!
//! The prototype deliberately keeps persistence and Git projection outside this
//! crate. Its executable contract is stable logical change identity, immutable
//! revisions, deterministic descendant reapplication, and durable conflicts.

mod collection;
mod collection_builders;
mod hydration;
mod id;
mod model;
mod persisted;
mod repository;
mod root_builders;
mod semantic_records;
mod wire;

pub use collection::{
    ApplicationMapChild, ApplicationMapNode, ConflictSetChild, ConflictSetNode, ContentSetChild,
    ContentSetNode, RetainedSetChild, RetainedSetNode,
};
pub use collection_builders::{
    build_application_map, build_conflict_set, build_content_set, build_retained_set,
    build_revision_map,
};
pub use hydration::{hydrate_application, hydrate_revision};
pub use id::{
    ApplicationId, ApplicationMapId, ChangeId, ChangeObjectId, ConflictId, ConflictSetId,
    ContentId, ContentManifestId, ContentSetId, ObjectId, ParseContentIdError, ParseDigestIdError,
    RepositoryStateId, RetainedSetId, RevisionId, RevisionMapId, TreeId, ViewId,
};
pub use model::{
    Actor, Application, ApplicationResult, CanonicalObject, Change, ChangeMetadata, ChangeRevision,
    Conflict, ConflictResolution, DependencyGraph, DirectoryEntry, DirectoryNode, FileEdit,
    PathConflict, RepositoryClosure, RepositoryState, RepositoryView, RevisionMapChild,
    RevisionMapNode, RevisionMetadata, SourceProvenance, Timestamp, Transformation, Tree,
};
pub use persisted::{ContentManifestRecord, RepositoryStateRecord, ViewRecord};
pub use repository::{
    ChangeProposal, Repository, RepositoryError, RepositoryParts, RevisionOutcome,
    TargetPolicyResult,
};
pub use root_builders::{
    build_content_manifest_record, build_repository_state_record, build_view_record,
};
pub use semantic_records::{
    ApplicationRecord, ApplicationResultRecord, RevisionRecord, decode_application_record,
    decode_change, decode_conflict, decode_revision_record,
};
pub use wire::{
    CanonicalObjectError, CanonicalObjectKind, decode_application_map_node,
    decode_conflict_set_node, decode_content_manifest_record, decode_content_set_node,
    decode_directory_node, decode_repository_state_record, decode_retained_set_node,
    decode_revision_map_node, decode_view_record, validate_canonical_object,
    validate_directory_component,
};
