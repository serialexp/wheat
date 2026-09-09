//! Local content-addressed storage for the Wheat prototype.
//!
//! Repository model identities remain in `wheat-core`; this crate owns only
//! byte retention and integrity verification for those identities.

mod application_map;
mod batch;
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
mod packed;
pub use packed::PackedObjectStore;
mod publication;
mod radix;
mod repository_envelope;
mod repository_hydration;
mod repository_mutation;
mod repository_publication;
mod repository_reader;
mod repository_transition;
mod repository_verify;
mod verification_scratch;
pub use repository_reader::RepositoryReader;
mod revision_map;
mod sets;

pub use application_map::{ApplicationMapCursor, ApplicationMapError, ApplicationMapStore};
pub use batch::BatchObjectStore;
pub use commit_writer::{CommitWriterError, PersistedCommit, PersistedCommitWriter};
pub use durability::{Durability, sync_directory, sync_filesystem};
pub use loose::{
    ImmutableObjectStore, LooseObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};
pub use merkle_tree::{MerkleTreeError, MerkleTreeStore, TreeEdit};
pub use mutation_publication::{
    PreparedMutationPublication, finalize_mutation_publication, prepare_mutation_publication,
};
pub use object_store_adapter::{
    ObjectStoreAdapter, ObjectStoreAdapterBuildError, S3ObjectStoreConfig,
};
pub use publication::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};
pub use repository_mutation::{
    MutationTarget, PersistedConflictResolution, PersistedRepositoryMutator,
    PreparedRepositoryMutation, RepositoryMutationError,
};
pub use repository_publication::{RepositoryPublicationError, RepositoryPublisher};
pub use revision_map::{RevisionMapCursor, RevisionMapError, RevisionMapStore};
pub use sets::{
    CollectionError, ConflictSetCursor, ConflictSetStore, ContentSetCursor, ContentSetStore,
    RetainedSetCursor, RetainedSetStore,
};

mod content_store;
pub use content_store::{ContentStore, ContentStoreError};
