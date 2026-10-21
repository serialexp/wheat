//! Git-independent source-control primitives for Wheat.
//!
//! The prototype deliberately keeps persistence and Git projection outside this
//! crate. Its executable contract is stable logical change identity, immutable
//! revisions, deterministic descendant reapplication, and durable conflicts.

mod branch_records;
mod collection;
mod collection_builders;
mod content_generation_wire;
mod content_io;
pub mod diff_model;
pub mod edit_application;
pub mod edit_capture;
pub mod edit_placement;
mod edit_records;
pub mod guarded;
mod hydration;
mod id;
pub mod message;
mod model;
mod persisted;
mod repository;
mod root_builders;
mod segment_records;
mod semantic_records;
mod store_records;
pub mod text_capture;
mod verified_content;
mod wire;

pub use branch_records::{
    LabelAnnotation, MAX_LABEL_ANNOTATION_MESSAGE_BYTES, RepositoryEnvelopeRecord, ViewLabel,
    ViewLabelManifestRecord,
};
pub use collection::{
    ApplicationMapChild, ApplicationMapNode, ChangeMapChild, ChangeMapNode, ConflictSetChild,
    ConflictSetNode, ContentMapChild, ContentMapNode, ContentSetChild, ContentSetNode,
    ProjectedCommitSetChild, ProjectedCommitSetNode, RetainedSetChild, RetainedSetNode,
    ViewLineageMapChild, ViewLineageMapNode,
};
pub use collection_builders::{
    build_application_map, build_change_map, build_conflict_set, build_content_map,
    build_content_set, build_projected_commit_set, build_retained_set, build_revision_map,
    build_view_lineage_map,
};
pub use content_generation_wire::{
    CONTENT_GENERATION_SHARD_ENTRIES, ContentGenerationWireEntry, ContentGenerationWireError,
    ContentGenerationWireIndex, ContentGenerationWireShard, ContentGenerationWireShardRef,
    content_generation_fingerprint, content_generation_index_digest,
    decode_content_generation_index, decode_content_generation_shard,
    encode_content_generation_index, encode_content_generation_shard,
};
pub use content_io::{ApplicationContext, ContentIo};
pub use edit_records::{EditOperation, EditRecord, decode_edit_record};
pub use hydration::{hydrate_application, hydrate_revision};
pub use id::{
    ApplicationId, ApplicationMapId, ChangeId, ChangeMapId, ChangeObjectId, ConflictId,
    ConflictSetId, ContentId, ContentMapId, ContentSetId, EditId, ObjectId, ParseContentIdError,
    ParseDigestIdError, ProjectedCommit, ProjectedCommitSetId, RepositoryEnvelopeId,
    RepositoryStateId, RetainedSetId, RevisionId, RevisionMapId, TreeId, ViewId,
    ViewLabelManifestId, ViewLineageMapId, content_bytes_hashed,
};
pub use model::{
    Actor, Application, ApplicationOrder, ApplicationResult, AppliedEditEvidence, ByteRange,
    CanonicalObject, Change, ChangeMetadata, ChangeRevision, Conflict, ConflictResolution,
    ConflictResolutionEvidence, DependencyGraph, DirectoryEntry, DirectoryNode, EditFailure,
    EditSubsumption, EditTransition, FailedEditEvidence, FileKind, FileValue, MAX_COMBINES,
    OrderIndexMismatch, RangeMax, RejectedEdit, RepositoryClosure, RepositoryState, RepositoryView,
    RevisionEdits, RevisionMapChild, RevisionMapNode, RevisionMetadata, SourceProvenance,
    Timestamp, Tree, ViewOrder, next_generation,
};
pub use persisted::{RepositoryStateRecord, ViewRecord};
pub use repository::{
    ChangeProposal, Repository, RepositoryError, RepositoryParts, RevisionOutcome, RevisionRequest,
    RevisionsOutcome, TargetPolicyResult,
};
pub use root_builders::{build_repository_state_record, build_view_record};
pub use segment_records::{
    ACTIVE_LOG_FRAME_HEADER_ENCODED_SIZE, ACTIVE_LOG_FRAME_TRAILER_ENCODED_SIZE,
    ACTIVE_LOG_HEADER_ENCODED_SIZE, ActiveLogFrameFields, MAX_PHYSICAL_CATALOG_SEGMENTS,
    PhysicalCatalogRef, PhysicalCatalogSnapshot, PhysicalContentGeneration,
    PhysicalSegmentDescriptor, RepackIntent, SEGMENT_DELTA_INDEX_ENTRY_ENCODED_SIZE,
    SEGMENT_FOOTER_ENCODED_SIZE, SEGMENT_INDEX_ENTRY_ENCODED_SIZE, SEGMENT_MAX_DELTA_DEPTH,
    SEGMENT_NAMESPACE_CONTENT, SEGMENT_NAMESPACE_METADATA, SEGMENT_PREFIX_ENCODED_SIZE,
    SegmentCatalogRecord, SegmentCodec, SegmentDirectory, SegmentDirectoryEntry, SegmentFooter,
    SegmentId, SegmentIndexEntry, SegmentIndexKey, SegmentIndexPage, SegmentRecordError,
    decode_active_log_frame_header, decode_active_log_frame_trailer, decode_active_log_header,
    decode_physical_catalog_head, decode_physical_catalog_snapshot, decode_repack_intent,
    decode_segment_catalog_record, decode_segment_directory, decode_segment_footer,
    decode_segment_index_page, decode_segment_prefix, encode_active_log_frame_fields,
    encode_active_log_frame_header, encode_active_log_frame_trailer, encode_active_log_header,
    encode_physical_catalog_head, encode_physical_catalog_snapshot, encode_repack_intent,
    encode_segment_catalog_record, encode_segment_directory, encode_segment_footer,
    encode_segment_index_page, encode_segment_prefix,
};
pub use semantic_records::{
    ApplicationRecord, ApplicationResultRecord, RevisionRecord, decode_application_record,
    decode_change, decode_conflict, decode_revision_record,
};
pub use store_records::{
    CONTENT_ENVELOPE_HEADER_ENCODED_SIZE, ContentEnvelopeHeaderRecord,
    INVENTORY_ENTRY_ENCODED_SIZE, InventoryRecord, MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE,
    MAX_PUBLISH_RECEIPTS, MaterializationProgressFields, PUBLISH_RECEIPT_ENCODED_SIZE,
    PublishReceiptRecord, REPOSITORY_HEAD_ENCODED_SIZE, RepositoryHeadRecord,
    SNAPSHOT_MANIFEST_FIXED_ENCODED_SIZE, SnapshotManifestRecord, StoreRecordError,
    decode_content_envelope_header, decode_materialization_progress_record, decode_repository_head,
    decode_snapshot_manifest, encode_content_envelope_header, encode_git_import_source,
    encode_materialization_progress_fields, encode_materialization_progress_record,
    encode_ownership_manifest, encode_repository_head, encode_snapshot_manifest,
    publish_key_digest, publish_receipt_id, publish_request_digest,
};
pub use verified_content::{ContentMismatch, VerifiedContent};
pub use wire::{
    CanonicalObjectError, CanonicalObjectKind, decode_application_map_node, decode_change_map_node,
    decode_conflict_set_node, decode_content_map_node, decode_content_set_node,
    decode_directory_node, decode_projected_commit_set_node, decode_repository_envelope_record,
    decode_repository_state_record, decode_retained_set_node, decode_revision_map_node,
    decode_view_label_manifest_record, decode_view_lineage_map_node, decode_view_record,
    validate_canonical_object, validate_directory_component,
};
