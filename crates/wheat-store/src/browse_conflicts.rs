//! Reads of a view's unresolved conflicts for a host's pages: a page of them in
//! canonical order, each with its change's title and its failed edits, and one
//! conflict's per-path detail with the three files a resolution is decided from.
//! [`ConflictIndex`] finds the conflicts, so a read costs what it shows plus O(log n)
//! index rows, not the view's history.

use std::{collections::BTreeMap, num::NonZeroUsize};

use wheat_core::{
    ApplicationId, ApplicationRecord, ApplicationResultRecord, ChangeId, ConflictId, EditFailure,
    EditId, EditOperation, FileValue, RevisionId, RevisionRecord, TreeId, ViewId,
};

use crate::{
    BrowseError, ChangeMapStore, ConflictIndex, ImmutableObjectStore, IndexedConflict,
    MerkleTreeStore, RepositoryBrowser, RepositoryPublicationError,
};

/// One failed edit of a conflict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictEditItem {
    pub edit: EditId,
    pub path: String,
    pub reason: EditFailure,
    /// The file the edit found at its path where it was applied, if any.
    pub found: Option<FileValue>,
    /// What the edit wanted.
    pub operation: EditOperation,
}

/// One unresolved conflict of a view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictItem {
    /// The conflicted change's index in the view's canonical order.
    pub position: u64,
    pub conflict: ConflictId,
    pub application: ApplicationId,
    pub change: ChangeId,
    pub revision: RevisionId,
    pub title: String,
    /// The view the change was applied onto: the conflict's base view, where the
    /// failed edits found what they found.
    pub target_view: ViewId,
    /// How many edits failed.
    pub edit_count: usize,
    /// The failed edits by ID, all of them unless the page's edit budget ran out on
    /// this conflict.
    pub edits: Vec<ConflictEditItem>,
}

/// A page of [`RepositoryBrowser::conflicts`], earliest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConflictPage {
    pub items: Vec<ConflictItem>,
    /// The `after` for the next page; none after the last conflict.
    pub next: Option<u64>,
}

/// How much a page of conflicts holds.
#[derive(Clone, Copy, Debug)]
pub struct ConflictPageLimits {
    pub conflicts: NonZeroUsize,
    /// How many failed edits the page reads in all. A page always holds at least one
    /// conflict: one with more edits holds the first of them and ends the page.
    pub edits: NonZeroUsize,
}

/// How an edit of the conflicted change fared on a path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictEditOutcome {
    Applied,
    Failed(EditFailure),
}

/// One edit of the conflicted change on a conflicted path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPathEdit {
    pub edit: EditId,
    pub operation: EditOperation,
    pub outcome: ConflictEditOutcome,
}

/// A conflicted path and the three files a resolution is decided from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPath {
    pub path: String,
    /// The file where the change was written: in its revision's `authored_against`.
    pub base: Option<FileValue>,
    /// The file the change wrote: in its revision's `resulting_tree`.
    pub theirs: Option<FileValue>,
    /// The file the edits met where the change landed: in the application's target
    /// view. A resolution replaces this one.
    pub found: Option<FileValue>,
    /// The change's edits on the path, failed and applied, by ID.
    pub edits: Vec<ConflictPathEdit>,
}

/// One conflict's detail from [`RepositoryBrowser::conflict`], paged by path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictDetail {
    pub position: u64,
    pub conflict: ConflictId,
    pub application: ApplicationId,
    pub change: ChangeId,
    pub revision: RevisionId,
    pub title: String,
    /// The view the change was applied onto, where `found` is read.
    pub target_view: ViewId,
    pub authored_against: ViewId,
    pub resulting_tree: TreeId,
    /// How many distinct paths the conflict's failed edits name.
    pub path_count: usize,
    /// The page's paths in byte order.
    pub paths: Vec<ConflictPath>,
    /// The `after_path` for the next page; none after the last path.
    pub next_path: Option<String>,
}

/// A conflict's records, read and checked against each other.
struct ConflictRecords {
    application: ApplicationRecord,
    revision: RevisionRecord,
    conflict: wheat_core::Conflict,
    title: String,
}

impl<O: ImmutableObjectStore> RepositoryBrowser<'_, O> {
    /// The number of `view`'s unresolved conflicts; see [`ConflictIndex::count`].
    pub async fn conflict_count(
        &self,
        index: &ConflictIndex,
        view: ViewId,
    ) -> Result<u64, BrowseError> {
        let record = self.view(view).await?;
        Ok(index.count(&self.reader, &record).await?)
    }

    /// `view`'s unresolved conflicts after canonical position `after` (from the first
    /// when `None`), earliest first, within `limits`. Each conflict costs its
    /// application, revision, conflict and change reads, and one read per edit on
    /// the page.
    #[tracing::instrument(name = "browse_conflicts", level = "debug", skip_all)]
    pub async fn conflicts(
        &self,
        index: &ConflictIndex,
        view: ViewId,
        after: Option<u64>,
        limits: ConflictPageLimits,
    ) -> Result<ConflictPage, BrowseError> {
        let record = self.view(view).await?;
        let page = index
            .conflicts(&self.reader, &record, after, limits.conflicts)
            .await?;
        let count = page.conflicts.len();
        let mut budget = limits.edits.get();
        let mut items = Vec::with_capacity(count);
        for (index, indexed) in page.conflicts.into_iter().enumerate() {
            let records = self.conflict_records(indexed).await?;
            let edit_count = records.conflict.edits.len();
            let mut edits = Vec::with_capacity(edit_count.min(budget));
            for (edit, evidence) in records.conflict.edits.iter().take(budget) {
                let record = self.reader.edit(*edit).await?;
                edits.push(ConflictEditItem {
                    edit: *edit,
                    path: record.path,
                    reason: evidence.reason.clone(),
                    found: evidence.target,
                    operation: record.operation,
                });
            }
            budget -= edits.len();
            let truncated = edits.len() < edit_count;
            items.push(ConflictItem {
                position: indexed.position,
                conflict: indexed.conflict,
                application: indexed.application,
                change: indexed.change,
                revision: records.application.revision_id,
                title: records.title,
                target_view: records.application.target_view,
                edit_count,
                edits,
            });
            let more = index + 1 < count || page.next.is_some();
            if (truncated || budget == 0) && more {
                return Ok(ConflictPage {
                    items,
                    next: Some(indexed.position),
                });
            }
        }
        Ok(ConflictPage {
            items,
            next: page.next,
        })
    }

    /// The conflict at canonical position `position` in `view`, with up to `limit` of
    /// its paths after `after_path` (from the first when `None`); `None` when the
    /// change there has no unresolved conflict. Reads every edit of the conflict and
    /// of the change's application to group them by path, so it costs what the
    /// change's edits number, and three tree lookups for the page's paths.
    #[tracing::instrument(name = "browse_conflict", level = "debug", skip_all)]
    pub async fn conflict(
        &self,
        index: &ConflictIndex,
        view: ViewId,
        position: u64,
        after_path: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<Option<ConflictDetail>, BrowseError> {
        let record = self.view(view).await?;
        let Some(indexed) = index.at(&self.reader, &record, position).await? else {
            return Ok(None);
        };
        let records = self.conflict_records(indexed).await?;
        let mut paths = BTreeMap::<String, Vec<ConflictPathEdit>>::new();
        for (edit, evidence) in &records.conflict.edits {
            let record = self.reader.edit(*edit).await?;
            paths
                .entry(record.path)
                .or_default()
                .push(ConflictPathEdit {
                    edit: *edit,
                    operation: record.operation,
                    outcome: ConflictEditOutcome::Failed(evidence.reason.clone()),
                });
        }
        for edit in records.application.applied.keys() {
            let record = self.reader.edit(*edit).await?;
            if let Some(edits) = paths.get_mut(&record.path) {
                edits.push(ConflictPathEdit {
                    edit: *edit,
                    operation: record.operation,
                    outcome: ConflictEditOutcome::Applied,
                });
            }
        }
        let path_count = paths.len();
        let mut page = paths
            .into_iter()
            .filter(|(path, _)| after_path.is_none_or(|after| path.as_str() > after))
            .take(limit.get() + 1)
            .collect::<Vec<_>>();
        let next_path = (page.len() > limit.get()).then(|| {
            page.pop();
            page.last().map(|(path, _)| path.clone())
        });
        let names = page
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        let trees = MerkleTreeStore::new(self.reader.objects);
        let base_tree = self
            .reader
            .view(records.revision.authored_against)
            .await?
            .tree;
        let found_tree = self
            .reader
            .view(records.application.target_view)
            .await?
            .tree;
        let base = trees.lookup_many(base_tree, &names).await?;
        let theirs = trees
            .lookup_many(records.revision.resulting_tree, &names)
            .await?;
        let found = trees.lookup_many(found_tree, &names).await?;
        let paths = page
            .into_iter()
            .zip(base.into_iter().zip(theirs).zip(found))
            .map(|((path, mut edits), ((base, theirs), found))| {
                edits.sort_by_key(|edit| edit.edit);
                ConflictPath {
                    path,
                    base,
                    theirs,
                    found,
                    edits,
                }
            })
            .collect();
        Ok(Some(ConflictDetail {
            position,
            conflict: indexed.conflict,
            application: indexed.application,
            change: indexed.change,
            revision: records.application.revision_id,
            title: records.title,
            target_view: records.application.target_view,
            authored_against: records.revision.authored_against,
            resulting_tree: records.revision.resulting_tree,
            path_count,
            paths,
            next_path: next_path.flatten(),
        }))
    }

    /// Reads the application, revision, conflict and change behind `indexed`, and
    /// checks they are the ones it names.
    async fn conflict_records(
        &self,
        indexed: IndexedConflict,
    ) -> Result<ConflictRecords, BrowseError> {
        let application = self.reader.application(indexed.application).await?;
        if application.id() != indexed.application || application.order.position != indexed.position
        {
            return Err(BrowseError::IdentityMismatch("application"));
        }
        match application.result {
            ApplicationResultRecord::Conflicted { conflict, .. }
                if conflict == indexed.conflict => {}
            _ => return Err(BrowseError::IdentityMismatch("application conflict")),
        }
        let revision = self.reader.revision(application.revision_id).await?;
        if revision.id() != application.revision_id || revision.change_id != indexed.change {
            return Err(BrowseError::IdentityMismatch("revision"));
        }
        let conflict = self.reader.conflict(indexed.conflict).await?;
        if conflict.id != indexed.conflict || conflict.base_view != application.target_view {
            return Err(BrowseError::IdentityMismatch("conflict"));
        }
        let change_object = ChangeMapStore::new(self.reader.objects)
            .get(self.state.changes, indexed.change)
            .await
            .map_err(RepositoryPublicationError::from)?
            .ok_or(BrowseError::IdentityMismatch("change map"))?;
        let change = self.reader.change(change_object).await?;
        if change.id != indexed.change {
            return Err(BrowseError::IdentityMismatch("change"));
        }
        Ok(ConflictRecords {
            application,
            revision,
            conflict,
            title: change.metadata.title,
        })
    }
}
