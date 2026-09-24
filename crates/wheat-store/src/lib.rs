//! Local content-addressed storage for the Wheat prototype.
//!
//! Repository model identities remain in `wheat-core`; this crate owns only
//! byte retention and integrity verification for those identities.

mod application_map;
mod batch;
mod change_map;
mod closure;
mod commit_content;
mod commit_writer;
mod content_batch;
pub use content_batch::ContentWriteBatch;
mod content_generation;
pub use content_generation::{
    CONTENT_GENERATION_SHARD_ENTRIES, ContentGenerationEntry, ContentGenerationIndex,
    ContentGenerationManifest, ContentGenerationShardRef, content_generation_index_path,
    content_generation_shard_manifest_path, content_generation_shard_pack_path,
    read_content_generation, read_content_generation_index,
};
mod content_repack;
mod content_set;
pub use content_repack::{
    ContentGenerationTarget, ContentHistoryPair, ContentRepackAnalysis, ContentRepackError,
    ContentRepackInput, ContentRepackLimits, ContentRepackPlan, GenerationQualifiedEnvelope,
    PreparedContentGeneration, plan_content_repack, prepare_content_repack, publish_content_repack,
};
mod codec;
mod content_io;
mod delta;
mod durability;
mod loose;
mod merkle_tree;
mod metadata_repack;
mod mutation_publication;
mod object_store_adapter;
mod ownership;
pub use ownership::{
    LivenessProof, Owner, OwnerId, OwnerKind, OwnershipError, OwnershipLimits, OwnershipManifest,
    OwnershipManifestId, prove_union_liveness, prove_union_liveness_bounded,
    prove_union_liveness_with,
};
mod packed;
pub use packed::{
    PackedObjectStore, PackedStoreTrace, PackedStoreTraceSnapshot, RepackError, RepackOptions,
    RepackPlan, RepackReport,
};
mod remote_packed;
pub use remote_packed::{RemotePackedStore, RemotePackedStoreConfig};
mod publication;
mod radix;
mod repository_envelope;
mod repository_hydration;
mod repository_mutation;
mod repository_publication;
mod repository_reader;
mod repository_rewrite;
mod repository_transition;
mod repository_verify;
pub use repository_verify::{RepositoryValidationPhase, TransitionValidationOptions};
mod verification_scratch;
pub use repository_reader::RepositoryReader;
mod revision_map;
mod sets;
mod snapshot;
pub use snapshot::{
    ContentBatch, InventoryEntry, MemorySnapshotSink, NativeSnapshot, ObjectBatch, SnapshotError,
    SnapshotId, SnapshotLimits, SnapshotManifest, SnapshotSink, SnapshotSource, export_snapshot,
    export_snapshot_to, restore_snapshot, restore_snapshot_from,
};
mod view_lineage;

pub use application_map::{
    ApplicationMapCursor, ApplicationMapDelta, ApplicationMapDeltaLimits, ApplicationMapError,
    ApplicationMapStore,
};
pub use batch::BatchObjectStore;
pub use change_map::{
    ChangeMapCursor, ChangeMapDelta, ChangeMapDeltaLimits, ChangeMapError, ChangeMapStore,
};
pub use closure::collect_structural_closure;
pub use commit_writer::{CommitWriterError, PersistedCommit, PersistedCommitWriter};
pub use content_set::{ContentSetCursor, ContentSetDelta, ContentSetStore};
pub use durability::{Durability, sync_directory, sync_filesystem};
pub use loose::{
    ImmutableObjectStore, LooseObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};
pub use merkle_tree::{
    FinalTreeUpdateLimits, MerkleTreeError, MerkleTreeStore, TreeContentCursor,
    TreeContentWalkLimits, TreeDiff, TreeDiffCursor, TreeDiffLimits, TreeEdit, TreeFile,
    TreePathLimits, TreePathSelection, TreePathShape, TreeSelectionLimits, TreeWalkLimits,
};
pub use mutation_publication::{
    PreparedMutationPublication, finalize_mutation_publication, prepare_mutation_publication,
};
pub use object_store_adapter::{
    ObjectStoreAdapter, ObjectStoreAdapterBuildError, S3ObjectStoreConfig,
};
pub use publication::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};
pub use repository_mutation::{
    PersistedConflictResolution, PersistedRepositoryMutator, PreparedRepositoryMutation,
    RepositoryMutationError,
};
pub use repository_publication::{RepositoryPublicationError, RepositoryPublisher};
pub use repository_rewrite::{
    PreparedRepositoryRewrite, RepositoryRewriteError, RepositoryRewriter, RewriteMapping,
    SquashRewriteRequest, finalize_repository_rewrite,
};
pub use revision_map::{
    RevisionMapCursor, RevisionMapDelta, RevisionMapDeltaLimits, RevisionMapError, RevisionMapStore,
};
pub use sets::{
    CollectionError, ConflictSetCursor, ConflictSetStore, ContentSetDeltaLimits, RetainedSetCursor,
    RetainedSetDelta, RetainedSetDeltaLimits, RetainedSetStore,
};
pub use view_lineage::{
    ViewLineageError, ViewLineageLimits, ViewLineageMapStore, ViewLineageValidation,
};

mod content_store;
pub use content_store::{ContentStore, ContentStoreError};

/// Read-only segment and loose-object access for analysis tools (e.g.
/// `wheat store analyze`).
#[doc(hidden)]
pub mod packed_segment_reader {
    use std::fs::File;
    use std::path::Path;

    use crate::codec;
    use crate::packed::segment::SegmentReader;

    /// Authenticated segment view returned by [`open_segment`].
    pub struct AuthenticatedSegment {
        reader: SegmentReader,
    }

    /// A single index entry exposed for measurement.
    pub struct SegmentEntry {
        pub namespace: u8,
        pub kind: u16,
        pub codec: u8,
        pub digest: [u8; 32],
        pub offset: u64,
        pub logical_length: u64,
        pub stored_length: u64,
        /// In-segment delta base, for metadata stored as a delta.
        pub delta_base: Option<[u8; 32]>,
        /// Links to the chain's whole record; 0 for whole entries.
        pub delta_depth: u8,
    }

    impl AuthenticatedSegment {
        /// Returns all authenticated index entries.
        pub fn entries(&self) -> Vec<SegmentEntry> {
            self.reader
                .authenticated_entries()
                .iter()
                .map(|e| SegmentEntry {
                    namespace: e.namespace,
                    kind: e.kind,
                    codec: e.codec,
                    digest: e.digest,
                    offset: e.offset,
                    logical_length: e.logical_length,
                    stored_length: e.stored_length,
                    delta_base: e.delta_base,
                    delta_depth: e.delta_depth,
                })
                .collect()
        }

        /// Reads and verifies one indexed payload, bounded before allocation.
        pub fn read_entry_bounded(
            &mut self,
            namespace: u8,
            digest: [u8; 32],
            max: usize,
        ) -> Result<Option<Vec<u8>>, String> {
            self.reader
                .lookup_entry_bounded(namespace, digest, max)
                .map(|entry| entry.map(|(_, payload)| payload))
                .map_err(|error| error.to_string())
        }

        /// Releases decompressed shared-block scratch during bounded traversals.
        pub fn clear_block_cache(&mut self) {
            self.reader.clear_block_cache();
        }
    }

    /// Opens and authenticates a segment file.
    pub fn open_segment(file: File) -> Result<AuthenticatedSegment, String> {
        Ok(AuthenticatedSegment {
            reader: SegmentReader::open(file).map_err(|e| e.to_string())?,
        })
    }

    /// Read a loose envelope file and return the decompressed canonical bytes.
    ///
    /// This is the synchronous counterpart of `LooseObjectStore::get` exposed
    /// for analysis tools that need to identify object kinds without an async
    /// runtime.
    pub fn read_loose_envelope(path: &Path) -> Result<Vec<u8>, String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let physical_length = file.metadata().map_err(|e| e.to_string())?.len();
        codec::read_envelope_file(&mut file, physical_length, codec::HARD_LOGICAL_RECORD_BYTES)
            .map_err(|e| format!("{e}"))
    }
}

/// Test helpers for integration tests that need to build segments directly.
#[doc(hidden)]
pub mod packed_segment_test_helpers {
    use wheat_core::{CanonicalObject, validate_canonical_object};

    use crate::packed::segment::{
        METADATA_NAMESPACE, SegmentBuilder, SegmentRecord, prepare_record,
    };

    /// Build a segment from canonical closure objects (with block compression)
    /// and return its serialized bytes.
    pub fn build_segment_bytes(objects: &[CanonicalObject]) -> Vec<u8> {
        use std::io::Read;
        let mut builder = SegmentBuilder::new(std::env::temp_dir()).unwrap();
        for object in objects {
            let kind = validate_canonical_object(&object.bytes).unwrap();
            let kind_code = wheat_core::SegmentIndexEntry::canonical_kind_code(kind);
            let record = SegmentRecord {
                namespace: METADATA_NAMESPACE,
                kind: kind_code,
                digest: *object.id.as_bytes(),
                bytes: &object.bytes,
            };
            builder
                .push_prepared(prepare_record(record).unwrap())
                .unwrap();
        }
        let sealed = builder.seal().unwrap();
        let mut file = sealed.file.reopen().unwrap();
        let mut result = Vec::new();
        file.read_to_end(&mut result).unwrap();
        result
    }
}
