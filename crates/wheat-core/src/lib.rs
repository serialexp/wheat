//! Git-independent source-control primitives for Wheat.
//!
//! The prototype deliberately keeps persistence and Git projection outside this
//! crate. Its executable contract is stable logical change identity, immutable
//! revisions, deterministic descendant reapplication, and durable conflicts.

mod branch_records;
mod collection;
mod collection_builders;
mod content_io;
pub mod diff_model;
pub mod edit_application;
pub mod edit_capture;
mod edit_records;
pub mod guarded;
mod hydration;
mod id;
mod model;
mod persisted;
mod repository;
mod root_builders;
mod segment_records;
mod semantic_records;
pub mod text_capture;
mod wire;

pub use branch_records::{RepositoryEnvelopeRecord, ViewLabel, ViewLabelManifestRecord};
pub use collection::{
    ApplicationMapChild, ApplicationMapNode, ChangeMapChild, ChangeMapNode, ConflictSetChild,
    ConflictSetNode, ContentSetChild, ContentSetNode, RetainedSetChild, RetainedSetNode,
    ViewLineageMapChild, ViewLineageMapNode,
};
pub use collection_builders::{
    build_application_map, build_change_map, build_conflict_set, build_content_set,
    build_retained_set, build_revision_map, build_view_lineage_map,
};
pub use content_io::{ApplicationContext, ContentIo};
pub use edit_records::{EditOperation, EditRecord, decode_edit_record};
pub use hydration::{hydrate_application, hydrate_revision};
pub use id::{
    ApplicationId, ApplicationMapId, ChangeId, ChangeMapId, ChangeObjectId, ConflictId,
    ConflictSetId, ContentId, ContentManifestId, ContentSetId, EditId, ObjectId,
    ParseContentIdError, ParseDigestIdError, RepositoryEnvelopeId, RepositoryStateId,
    RetainedSetId, RevisionId, RevisionMapId, TreeId, ViewId, ViewLabelManifestId,
    ViewLineageMapId,
};
pub use model::{
    Actor, Application, ApplicationResult, AppliedEditEvidence, ByteRange, CanonicalObject, Change,
    ChangeMetadata, ChangeRevision, Conflict, ConflictResolution, ConflictResolutionEvidence,
    DependencyGraph, DirectoryEntry, DirectoryNode, EditFailure, EditSubsumption, EditTransition,
    FailedEditEvidence, FileKind, FileValue, RepositoryClosure, RepositoryState, RepositoryView,
    RevisionEdits, RevisionMapChild, RevisionMapNode, RevisionMetadata, SourceProvenance,
    Timestamp, Tree,
};
pub use persisted::{ContentManifestRecord, RepositoryStateRecord, ViewRecord};
pub use repository::{
    ChangeProposal, Repository, RepositoryError, RepositoryParts, RevisionOutcome,
    TargetPolicyResult,
};
pub use root_builders::{
    build_content_manifest_record, build_repository_state_record, build_view_record,
};
pub use segment_records::{
    SEGMENT_FOOTER_ENCODED_SIZE, SEGMENT_NAMESPACE_CONTENT, SEGMENT_NAMESPACE_METADATA,
    SEGMENT_PREFIX_ENCODED_SIZE, SegmentCatalogRecord, SegmentDirectory, SegmentDirectoryEntry,
    SegmentFooter, SegmentId, SegmentIndexEntry, SegmentIndexKey, SegmentIndexPage,
    SegmentRecordError, decode_segment_catalog_record, decode_segment_directory,
    decode_segment_footer, decode_segment_index_page, decode_segment_prefix,
    encode_segment_catalog_record, encode_segment_directory, encode_segment_footer,
    encode_segment_index_page, encode_segment_prefix,
};
pub use semantic_records::{
    ApplicationRecord, ApplicationResultRecord, RevisionRecord, decode_application_record,
    decode_change, decode_conflict, decode_revision_record,
};
pub use wire::{
    CanonicalObjectError, CanonicalObjectKind, ContentSetBlob, content_set_delta_bytes,
    content_set_snapshot_bytes, decode_application_map_node, decode_change_map_node,
    decode_conflict_set_node, decode_content_manifest_record, decode_content_set_blob,
    decode_content_set_node, decode_directory_node, decode_repository_envelope_record,
    decode_repository_state_record, decode_retained_set_node, decode_revision_map_node,
    decode_view_label_manifest_record, decode_view_lineage_map_node, decode_view_record,
    extract_object_references, validate_canonical_object, validate_directory_component,
};
