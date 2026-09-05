//! Git-independent source-control primitives for Wheat.
//!
//! The prototype deliberately keeps persistence and Git projection outside this
//! crate. Its executable contract is stable logical change identity, immutable
//! revisions, deterministic descendant reapplication, and durable conflicts.

mod collection;
mod id;
mod model;
mod repository;
mod wire;

pub use collection::{
    ApplicationMapChild, ApplicationMapNode, ConflictSetChild, ConflictSetNode, ContentSetChild,
    ContentSetNode, RetainedSetChild, RetainedSetNode,
};
pub use id::{
    ApplicationId, ApplicationMapId, ChangeId, ChangeObjectId, ConflictId, ConflictSetId, ContentId,
    ContentManifestId, ContentSetId, ObjectId, ParseContentIdError, ParseDigestIdError, RetainedSetId,
    RepositoryStateId, RevisionId, RevisionMapId, TreeId, ViewId,
};
pub use model::{
    Actor, Application, ApplicationResult, CanonicalObject, Change, ChangeMetadata, ChangeRevision,
    Conflict, ConflictResolution, DependencyGraph, DirectoryEntry, DirectoryNode, FileEdit,
    PathConflict, RepositoryClosure, RepositoryState, RepositoryView, RevisionMapChild,
    RevisionMapNode, RevisionMetadata, SourceProvenance, Timestamp, Transformation, Tree,
};
pub use repository::{
    ChangeProposal, Repository, RepositoryError, RevisionOutcome, TargetPolicyResult,
};
pub use wire::{
    CanonicalObjectError, CanonicalObjectKind, decode_application_map_node,
    decode_conflict_set_node, decode_content_set_node, decode_directory_node,
    decode_retained_set_node, decode_revision_map_node, validate_canonical_object,
    validate_directory_component,
};
