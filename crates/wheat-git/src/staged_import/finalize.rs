//! Atomic publication of a fully staged Git import.

mod content_accumulator;

use std::path::Path;

use content_accumulator::{ContentAccumulator, ContentAccumulatorError, RetainedTable};

use thiserror::Error;
use wheat_core::{
    ApplicationRecord, Change, ChangeObjectId, ContentId, ContentManifestRecord, EditOperation,
    EditRecord, ObjectId, RepositoryStateRecord, ViewLabel, ViewLabelManifestRecord,
};
use wheat_store::{
    CollectionError, CommitWriterError, ContentSetStore, ContentStore, ImmutableObjectStore,
    ObjectKind, ObjectStoreError, PublicationStore, RepositoryHead, RepositoryPublicationError,
    RepositoryPublisher, RepositoryReader, RetainedSetStore, ViewLineageMapStore,
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
    ViewLineage(#[from] wheat_store::ViewLineageError),
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
    #[error("view label manifest is invalid: {0}")]
    InvalidViewLabelManifest(String),
    #[error("Git import failed: {0}")]
    Git(String),
}

/// A boundary entered while finalizing a staged import.
///
/// This is experimental instrumentation for observing finalization without coupling callers to
/// its implementation. The observer is called synchronously before work in each phase begins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalizePhase {
    Preflight,
    JournalAuthenticationAccumulation,
    AggregateSetConstruction,
    BatchFlush,
    PrepareEnvelopeValidation,
    HeadInitialize,
}

impl FinalizePhase {
    /// Stable, machine-readable label for profiling output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::JournalAuthenticationAccumulation => "journal_auth_accumulation",
            Self::AggregateSetConstruction => "aggregate_set_construction",
            Self::BatchFlush => "batch_flush",
            Self::PrepareEnvelopeValidation => "prepare_envelope_validation",
            Self::HeadInitialize => "head_initialize",
        }
    }
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
    finalize_import_with_observer(
        git_path,
        journal_path,
        objects,
        content,
        publications,
        working_branch,
        |_| {},
    )
    .await
}

/// Finalize an import while synchronously observing phase boundaries.
///
/// The observer is never held across an await point.
pub async fn finalize_import_with_observer<O, P, F>(
    git_path: impl AsRef<Path>,
    journal_path: impl AsRef<Path>,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    working_branch: &str,
    observer: F,
) -> Result<RepositoryHead, FinalizeImportError>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
    F: FnMut(FinalizePhase),
{
    finalize_import_inner(
        git_path.as_ref(),
        journal_path.as_ref(),
        objects,
        content,
        publications,
        working_branch,
        observer,
        true,
    )
    .await
}

pub(crate) async fn finalize_import_deferred<O, P>(
    git_path: &Path,
    journal_path: &Path,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    working_branch: &str,
) -> Result<RepositoryHead, FinalizeImportError>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    finalize_import_inner(
        git_path,
        journal_path,
        objects,
        content,
        publications,
        working_branch,
        |_| {},
        false,
    )
    .await
}

async fn finalize_import_inner<O, P, F>(
    git_path: &Path,
    journal_path: &Path,
    objects: &O,
    content: &ContentStore,
    publications: &P,
    working_branch: &str,
    mut observer: F,
    durable_journal: bool,
) -> Result<RepositoryHead, FinalizeImportError>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
    F: FnMut(FinalizePhase),
{
    observer(FinalizePhase::Preflight);
    let staged = if durable_journal {
        stage_import(git_path, journal_path, objects, content, Some(0)).await?
    } else {
        super::stage_import_deferred(git_path, journal_path, objects, content, Some(0)).await?
    };
    if !staged.complete {
        return Err(FinalizeImportError::Incomplete {
            pending_commits: staged.pending_commits,
        });
    }
    staged
        .branches
        .get(working_branch)
        .ok_or_else(|| FinalizeImportError::UnknownWorkingBranch(working_branch.to_owned()))?
        .native
        .ok_or_else(|| FinalizeImportError::UnfinishedWorkingBranch(working_branch.to_owned()))?;

    // Re-pin and hold exclusive ownership from this point through head initialization.
    let git = gix::open(git_path).map_err(|e| FinalizeImportError::Git(e.to_string()))?;
    let pinned = pinned_branches(&git)?;
    let snapshot = snapshot_bytes(&git, git_path, &pinned)?;
    let journal = if durable_journal {
        ImportJournal::open(journal_path, &snapshot)?
    } else {
        ImportJournal::open_deferred(journal_path, &snapshot)?
    };

    let backing = objects;
    let batch = wheat_store::BatchObjectStore::new(backing, 1024, 8 * 1024 * 1024);
    let objects = &batch;
    let writer = wheat_store::PersistedCommitWriter::new(objects, content);
    let empty = writer.empty_view().await?;
    // Ordinary bounded staging authenticates crash-left work and the frontier it
    // consumes. Publication now re-pins Git metadata below and delegates complete
    // structural authentication to prepare_envelope before authority becomes visible.
    observer(FinalizePhase::JournalAuthenticationAccumulation);
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

    let mut change_objects = Vec::new();
    let mut view_edges = Vec::new();
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
            change_objects.push((native.change_id, change_id));
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
            if application.target_view != native.view_id {
                view_edges.push((application.target_view, native.view_id));
            }
            let view = reader
                .view(native.view_id)
                .await
                .map_err(|e| bad(e.to_string()))?;
            let manifest = reader
                .manifest(view.required_content)
                .await
                .map_err(|e| bad(e.to_string()))?;
            // Per-view manifests are already authenticated radix roots. Retain the root and
            // merge it structurally during finalization instead of enumerating every blob.
            ContentAccumulator::insert_manifest(&transaction, manifest.content)
                .map_err(|e| bad(e.to_string()))?;
            collect_historical_content(&transaction, &reader, &revision.edits, &application)
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

    observer(FinalizePhase::AggregateSetConstruction);
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

    let mut change_map_objects = Vec::new();
    let changes =
        wheat_core::build_change_map(change_objects, |object| change_map_objects.push(object));
    objects
        .put_batch(
            &change_map_objects
                .into_iter()
                .map(|object| (object.id, object.bytes))
                .collect::<Vec<_>>(),
            8,
        )
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
    let lineage = ViewLineageMapStore::new(objects);
    let view_lineage = lineage.empty_root().await?;
    let view_lineage = lineage.insert_edges(view_lineage, view_edges).await?;
    let state = RepositoryStateRecord {
        base_view: empty,
        changes,
        view_lineage,
        required_content: aggregate_manifest.id(),
        retained_changes,
        retained_views,
        retained_revisions,
        retained_applications,
    };
    objects
        .put_if_absent(state.id().into(), &state.canonical_bytes())
        .await?;

    let labels =
        ViewLabelManifestRecord::new(staged.branches.into_iter().map(|(original_ref, branch)| {
            let view = branch
                .native
                .expect("complete import has every branch tip")
                .view_id;
            let name = original_ref
                .strip_prefix("refs/heads/")
                .expect("staged branches are local branch refs")
                .to_owned();
            (
                name,
                ViewLabel {
                    view,
                    original_git_ref: Some(original_ref),
                },
            )
        }))
        .map_err(|e| FinalizeImportError::InvalidViewLabelManifest(e.to_string()))?;
    // Final validation/publication must read durable backing objects, not the
    // read-your-writes buffer used while constructing retained inventories.
    observer(FinalizePhase::BatchFlush);
    batch.flush().await?;
    let publisher = RepositoryPublisher::new(backing, content, publications);
    observer(FinalizePhase::PrepareEnvelopeValidation);
    let envelope = publisher.prepare_envelope(state.id(), &labels).await?;
    observer(FinalizePhase::HeadInitialize);
    Ok(publisher.initialize_prepared_envelope(envelope).await?)
}

/// Adds blobs referenced by historical revision records to the repository aggregate only.
///
/// Per-view manifests deliberately contain only their visible trees; definitions and applied
/// evidence remain durable through the repository state's aggregate content root.
async fn collect_historical_content<O: ImmutableObjectStore>(
    transaction: &rusqlite::Transaction<'_>,
    reader: &RepositoryReader<'_, O>,
    edits: &std::collections::BTreeSet<wheat_core::EditId>,
    application: &ApplicationRecord,
) -> Result<(), FinalizeImportError> {
    for edit_id in edits {
        let edit = reader.edit(*edit_id).await?;
        for id in edit_content(&edit) {
            ContentAccumulator::insert_content(transaction, id)?;
        }
    }
    for evidence in application.applied.values() {
        if let Some(value) = evidence.target {
            ContentAccumulator::insert_content(transaction, value.content)?;
        }
    }
    Ok(())
}

fn edit_content(edit: &EditRecord) -> impl Iterator<Item = ContentId> + '_ {
    let mut content = Vec::new();
    match &edit.operation {
        EditOperation::GuardedText {
            before,
            expected,
            replacement,
            after,
            ..
        } => content.extend([*before, *expected, *replacement, *after]),
        EditOperation::ExactFile {
            expected,
            replacement,
        } => content.extend(
            expected
                .iter()
                .chain(replacement)
                .map(|value| value.content),
        ),
    }
    content.into_iter()
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
