//! Atomic publication of a fully staged Git import.

mod content_accumulator;

use std::path::Path;

use content_accumulator::{ContentAccumulator, ContentAccumulatorError, RetainedTable};

use thiserror::Error;
use wheat_core::{
    BranchManifestRecord, Change, ChangeObjectId, ContentManifestRecord, ObjectId,
    RepositoryStateRecord,
};
use wheat_store::{
    CollectionError, CommitWriterError, ContentSetStore, ContentStore, ImmutableObjectStore,
    ObjectKind, ObjectStoreError, PublicationStore, RepositoryHead, RepositoryPublicationError,
    RepositoryPublisher, RepositoryReader, RetainedSetStore,
};

use crate::{
    journal::{ImportJournal, JournalError},
    metadata,
    staged_import::{StagedImportError, pinned_branches, snapshot_bytes, stage_import},
};

const PAGE: usize = 256;

#[derive(Debug, Error)]
pub enum FinalizeImportError {
    #[error(transparent)]
    Staging(#[from] StagedImportError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Object(#[from] ObjectStoreError),
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    Writer(#[from] CommitWriterError),
    #[error(transparent)]
    Publication(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Accumulator(#[from] ContentAccumulatorError),
    #[error("staged import is unfinished ({pending_commits} commits remain)")]
    Incomplete { pending_commits: usize },
    #[error("working branch {0:?} is not in the pinned Git snapshot")]
    UnknownWorkingBranch(String),
    #[error("working branch {0:?} has not been staged")]
    UnfinishedWorkingBranch(String),
    #[error("completed Git mapping for {oid} is invalid: {reason}")]
    InvalidMapping { oid: String, reason: String },
    #[error("branch manifest is invalid: {0}")]
    InvalidBranchManifest(String),
    #[error("Git import failed: {0}")]
    Git(String),
}

/// Finalize a complete staged import and initialize its repository head.
///
/// No commits are materialized here. `stage_import(..., Some(0))` only discovers work and
/// authenticates completed mappings; finalization rejects any remaining work.
pub async fn finalize_import<O, P>(
    git_path: impl AsRef<Path>,
    journal_path: impl AsRef<Path>,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    working_branch: &str,
) -> Result<RepositoryHead, FinalizeImportError>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    let git_path = git_path.as_ref();
    let journal_path = journal_path.as_ref();
    let staged = stage_import(git_path, journal_path, objects, content, Some(0)).await?;
    if !staged.complete {
        return Err(FinalizeImportError::Incomplete {
            pending_commits: staged.pending_commits,
        });
    }
    let selected = staged
        .branches
        .get(working_branch)
        .ok_or_else(|| FinalizeImportError::UnknownWorkingBranch(working_branch.to_owned()))?
        .native
        .ok_or_else(|| FinalizeImportError::UnfinishedWorkingBranch(working_branch.to_owned()))?;

    // Re-pin and hold exclusive ownership from this point through head initialization.
    let git = gix::open(git_path).map_err(|e| FinalizeImportError::Git(e.to_string()))?;
    let pinned = pinned_branches(&git)?;
    let snapshot = snapshot_bytes(&git, git_path, &pinned)?;
    let journal = ImportJournal::open(journal_path, &snapshot)?;

    let backing = objects;
    let batch = wheat_store::BatchObjectStore::new(backing, 1024, 8 * 1024 * 1024);
    let objects = &batch;
    let writer = wheat_store::PersistedCommitWriter::new(objects, content);
    let empty = writer.empty_view().await?;
    let reader = RepositoryReader::new(objects, content);
    let retained = RetainedSetStore::new(objects);
    let content_sets = ContentSetStore::new(objects);
    let mut accumulator = ContentAccumulator::new()?;
    {
        let transaction = accumulator.begin_page()?;
        ContentAccumulator::insert_retained(&transaction, RetainedTable::Views, empty.into())?;
        transaction
            .commit()
            .map_err(ContentAccumulatorError::from)?;
    }

    let mut after = None::<Vec<u8>>;
    loop {
        let page = journal.completed(after.as_deref(), PAGE)?;
        if page.is_empty() {
            break;
        }
        let transaction = accumulator.begin_page()?;
        for (oid, native) in page {
            let bad = |reason: String| FinalizeImportError::InvalidMapping {
                oid: hex(&oid),
                reason,
            };
            let commit = git
                .find_commit(gix::ObjectId::from_bytes_or_panic(&oid))
                .map_err(|e| bad(e.to_string()))?;
            let parents = commit
                .parent_ids()
                .map(|id| id.detach())
                .collect::<Vec<_>>();
            let (change_metadata, revision_metadata) =
                metadata(&commit, &oid, &parents).map_err(|e| bad(e.to_string()))?;
            let change = Change {
                id: native.change_id,
                metadata: change_metadata,
            };
            let change_id = ChangeObjectId::of_canonical_bytes(&change.canonical_bytes());
            authenticate_exact(
                objects,
                change_id.into(),
                ObjectKind::Change,
                &change.canonical_bytes(),
            )
            .await
            .map_err(|e| bad(e.to_string()))?;

            let revision = reader
                .revision(native.revision_id)
                .await
                .map_err(|e| bad(e.to_string()))?;
            if revision.change_id != native.change_id || revision.metadata != revision_metadata {
                return Err(bad("revision does not match the pinned Git commit".into()));
            }
            let application = reader
                .application(native.application_id)
                .await
                .map_err(|e| bad(e.to_string()))?;
            if application.revision_id != native.revision_id {
                return Err(bad("application does not select the mapped revision".into()));
            }
            let view = reader
                .view(native.view_id)
                .await
                .map_err(|e| bad(e.to_string()))?;
            let manifest = reader
                .manifest(view.required_content)
                .await
                .map_err(|e| bad(e.to_string()))?;
            ContentAccumulator::insert_manifest(&transaction, &content_sets, manifest.content)
                .await
                .map_err(|e| bad(e.to_string()))?;
            ContentAccumulator::insert_retained(
                &transaction,
                RetainedTable::Changes,
                change_id.into(),
            )?;
            ContentAccumulator::insert_retained(
                &transaction,
                RetainedTable::Views,
                native.view_id.into(),
            )?;
            ContentAccumulator::insert_retained(
                &transaction,
                RetainedTable::Revisions,
                native.revision_id.into(),
            )?;
            ContentAccumulator::insert_retained(
                &transaction,
                RetainedTable::Applications,
                native.application_id.into(),
            )?;
            after = Some(oid);
        }
        transaction
            .commit()
            .map_err(ContentAccumulatorError::from)?;
    }

    let all_content = accumulator.build_content(&content_sets).await?;
    let retained_changes = accumulator
        .build_retained(&retained, RetainedTable::Changes)
        .await?;
    let retained_views = accumulator
        .build_retained(&retained, RetainedTable::Views)
        .await?;
    let retained_revisions = accumulator
        .build_retained(&retained, RetainedTable::Revisions)
        .await?;
    let retained_applications = accumulator
        .build_retained(&retained, RetainedTable::Applications)
        .await?;

    let aggregate_manifest = ContentManifestRecord {
        content: all_content,
    };
    objects
        .put_if_absent(
            aggregate_manifest.id().into(),
            &aggregate_manifest.canonical_bytes(),
        )
        .await?;
    let state = RepositoryStateRecord {
        base_view: empty,
        working_view: selected.view_id,
        published_view: selected.view_id,
        required_content: aggregate_manifest.id(),
        retained_changes,
        retained_views,
        retained_revisions,
        retained_applications,
    };
    objects
        .put_if_absent(state.id().into(), &state.canonical_bytes())
        .await?;

    let branches = BranchManifestRecord::new(staged.branches.into_iter().map(|(name, branch)| {
        let view = branch
            .native
            .expect("complete import has every branch tip")
            .view_id;
        (name, view)
    }))
    .map_err(|e| FinalizeImportError::InvalidBranchManifest(e.to_string()))?;
    // Final validation/publication must read durable backing objects, not the
    // read-your-writes buffer used while constructing retained inventories.
    batch.flush().await?;
    let publisher = RepositoryPublisher::new(backing, content, publications);
    let envelope = publisher.prepare_envelope(state.id(), &branches).await?;
    Ok(publisher.initialize_prepared_envelope(envelope).await?)
}

async fn authenticate_exact<O: ImmutableObjectStore>(
    objects: &O,
    id: ObjectId,
    kind: ObjectKind,
    bytes: &[u8],
) -> Result<(), ObjectStoreError> {
    let stored = objects.get(id).await?;
    if stored.kind != kind {
        return Err(ObjectStoreError::WrongKind {
            id,
            expected: kind,
            actual: stored.kind,
        });
    }
    if stored.canonical_bytes != bytes {
        return Err(ObjectStoreError::Corrupt {
            expected: id,
            actual: ObjectId::of_canonical_bytes(&stored.canonical_bytes),
        });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
