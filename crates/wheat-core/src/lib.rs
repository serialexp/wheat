//! Git-independent source-control primitives for Wheat.
//!
//! The prototype deliberately keeps persistence and Git projection outside this
//! crate. Its executable contract is stable logical change identity, immutable
//! revisions, deterministic descendant reapplication, and durable conflicts.

mod id;
mod model;
mod repository;

pub use id::{ChangeId, ContentId, ParseContentIdError, RevisionId, ViewId};
pub use model::{
    Application, ApplicationResult, ChangeRevision, Conflict, ConflictResolution, DependencyGraph,
    FileEdit, PathConflict, RepositoryView, Transformation, Tree,
};
pub use repository::{Repository, RepositoryError, RevisionOutcome};
