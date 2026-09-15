//! Local content-addressed storage for the Wheat prototype.
//!
//! Repository model identities remain in `wheat-core`; this crate owns only
//! byte retention and integrity verification for those identities.

mod application_map;
mod batch;
mod change_map;
mod commit_content;
mod commit_writer;
mod content_batch;
pub use content_batch::ContentWriteBatch;
mod content_io;
mod durability;
mod loose;
mod merkle_tree;
mod mutation_publication;
mod object_store_adapter;
mod ownership;
pub use ownership::{
    LivenessProof, Owner, OwnerId, OwnerKind, OwnershipError, OwnershipLimits, OwnershipManifest,
    OwnershipManifestId, prove_union_liveness, prove_union_liveness_bounded,
    prove_union_liveness_with,
};
mod packed;
pub use packed::{PackedObjectStore, PackedStoreTrace, PackedStoreTraceSnapshot};
mod publication;
mod radix;
mod repository_envelope;
mod repository_hydration;
mod repository_mutation;
mod repository_publication;
mod repository_reader;
mod repository_transition;
mod repository_verify;
pub use repository_verify::RepositoryValidationPhase;
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

pub use application_map::{ApplicationMapCursor, ApplicationMapError, ApplicationMapStore};
pub use batch::BatchObjectStore;
pub use change_map::{ChangeMapCursor, ChangeMapError, ChangeMapStore};
pub use commit_writer::{CommitWriterError, PersistedCommit, PersistedCommitWriter};
pub use durability::{Durability, sync_directory, sync_filesystem};
pub use loose::{
    ImmutableObjectStore, LooseObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};
pub use merkle_tree::{
    MerkleTreeError, MerkleTreeStore, TreeDiff, TreeDiffCursor, TreeDiffLimits, TreeEdit, TreeFile,
    TreePathLimits, TreePathSelection, TreeSelectionLimits, TreeWalkLimits,
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
pub use revision_map::{
    RevisionMapCursor, RevisionMapDelta, RevisionMapDeltaLimits, RevisionMapError, RevisionMapStore,
};
pub use sets::{
    CollectionError, ConflictSetCursor, ConflictSetStore, ContentSetCursor, ContentSetDelta,
    ContentSetDeltaLimits, ContentSetStore, RetainedSetCursor, RetainedSetStore,
};
pub use view_lineage::{
    ViewLineageError, ViewLineageLimits, ViewLineageMapStore, ViewLineageValidation,
};

mod content_store;
pub use content_store::{ContentStore, ContentStoreError};
