//! Reads for browsing a repository without a workspace, as a host's repository pages
//! do: a label's view, a directory's entries, a file's bytes, and a view's history,
//! newest first in canonical order or by commit time, a page at a time. Every read is
//! bounded by its page or byte limit rather than by the repository's size.

use std::num::NonZeroUsize;

use thiserror::Error;
use wheat_core::{
    Actor, ApplicationId, ApplicationRecord, ApplicationResultRecord, ChangeId, FileValue,
    HistoryKey, RepositoryStateRecord, RevisionId, Timestamp, TreeId, ViewId,
    ViewLabelManifestRecord, ViewRecord,
};

use crate::{
    Blame, BlameError, BlameLimits, ChangeMapStore, ContentStore, ContentStoreError,
    HistoryMapError, HistoryMapStore, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore,
    PathHistoryIndex, PathHistoryIndexError, RepositoryHead, RepositoryPublicationError,
    RepositoryReader, RetainedSetStore, TreeListing,
};

#[derive(Debug, Error)]
pub enum BrowseError {
    #[error("view {0} is not retained by the repository head")]
    ViewNotRetained(ViewId),
    #[error("path {0:?} has an empty component")]
    InvalidPath(String),
    #[error("history position {position} is beyond the view's {size} changes")]
    PositionOutOfRange { position: u64, size: u64 },
    #[error("repository record {0} does not match its identity")]
    IdentityMismatch(&'static str),
    #[error(transparent)]
    Read(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Tree(#[from] MerkleTreeError),
    #[error(transparent)]
    History(#[from] HistoryMapError),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error(transparent)]
    PathHistory(#[from] PathHistoryIndexError),
    #[error(transparent)]
    Blame(#[from] BlameError),
}

/// A file read by [`RepositoryBrowser::read_file`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowsedFile {
    pub value: FileValue,
    /// The content's length in bytes.
    pub size: u64,
    /// The content, or `None` when it is longer than the read allowed.
    pub bytes: Option<Vec<u8>>,
}

/// One change in a view's history, at the revision the view holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryItem {
    /// The change's index in the view's canonical order.
    pub position: u64,
    pub application: ApplicationId,
    /// The view the change was applied onto: the one before it in the history.
    pub parent: ViewId,
    /// The tree after the change, partial when it conflicted.
    pub tree: TreeId,
    pub conflicted: bool,
    pub change: ChangeId,
    pub title: String,
    pub description: String,
    pub creator: Actor,
    pub revision: RevisionId,
    pub message: String,
    pub author: Actor,
    pub authored_at: Timestamp,
    pub committer: Actor,
    pub committed_at: Timestamp,
}

impl HistoryItem {
    /// The item's key in its view's history by time.
    #[must_use]
    pub fn time_key(&self) -> HistoryKey {
        HistoryKey::new(self.committed_at.seconds, self.change)
    }
}

/// A page of history in canonical order, newest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    /// The `before` for the next page; none after the oldest change.
    pub next: Option<u64>,
}

/// A page of history by commit time, newest first; within a second, by highest
/// change ID first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimeHistoryPage {
    pub items: Vec<HistoryItem>,
    /// The `from` for the next page, the key of its first change; none after the
    /// oldest change.
    pub next: Option<HistoryKey>,
}

/// Browsing reads at one repository head.
pub struct RepositoryBrowser<'a, O> {
    reader: RepositoryReader<'a, O>,
    state: RepositoryStateRecord,
    labels: ViewLabelManifestRecord,
}

impl<'a, O: ImmutableObjectStore> RepositoryBrowser<'a, O> {
    /// Reads `head`'s envelope, state and labels: three object reads.
    pub async fn open(
        objects: &'a O,
        content: &'a ContentStore,
        head: RepositoryHead,
    ) -> Result<Self, BrowseError> {
        let reader = RepositoryReader::new(objects, content);
        let envelope = reader.envelope(head.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let labels = reader.view_label_manifest(envelope.labels).await?;
        Ok(Self {
            reader,
            state,
            labels,
        })
    }

    #[must_use]
    pub fn labels(&self) -> &ViewLabelManifestRecord {
        &self.labels
    }

    /// The view `label` names, if the head has that label.
    #[must_use]
    pub fn label(&self, label: &str) -> Option<ViewId> {
        self.labels.labels.get(label).map(|label| label.view)
    }

    /// `view`'s record, refused unless the head retains it.
    pub async fn view(&self, view: ViewId) -> Result<ViewRecord, BrowseError> {
        if !RetainedSetStore::new(self.reader.objects)
            .contains(self.state.retained_views, view.into())
            .await
            .map_err(RepositoryPublicationError::from)?
        {
            return Err(BrowseError::ViewNotRetained(view));
        }
        let record = self.reader.view(view).await?;
        if record.id() != view {
            return Err(BrowseError::IdentityMismatch("view"));
        }
        Ok(record)
    }

    /// Lists the directory at `path` in `view` (`""` for the root), a page at a time;
    /// see [`MerkleTreeStore::list_directory`].
    pub async fn list(
        &self,
        view: ViewId,
        path: &str,
        after: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<TreeListing, BrowseError> {
        let tree = self.view(view).await?.tree;
        Ok(MerkleTreeStore::new(self.reader.objects)
            .list_directory(tree, &components(path)?, after, limit)
            .await?)
    }

    /// The file at `path` in `view`, with its bytes when it is at most `max_bytes`
    /// long; `None` when no file is there.
    pub async fn read_file(
        &self,
        view: ViewId,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<BrowsedFile>, BrowseError> {
        let tree = self.view(view).await?.tree;
        let components = components(path)?;
        if components.is_empty() {
            return Ok(None);
        }
        let Some(value) = MerkleTreeStore::new(self.reader.objects)
            .lookup(tree, &components)
            .await?
        else {
            return Ok(None);
        };
        let size = self.reader.content.logical_length(value.content)?;
        let bytes = if usize::try_from(size).is_ok_and(|size| size <= max_bytes) {
            Some(self.reader.content.get_bounded(value.content, max_bytes)?)
        } else {
            None
        };
        Ok(Some(BrowsedFile { value, size, bytes }))
    }

    /// Up to `limit` of `view`'s changes before canonical position `before` (all of
    /// them when `None`), newest first. Finding the first costs O(log n) object reads
    /// and each one after it a few more, so a page does not read the whole history.
    #[tracing::instrument(name = "browse_history", level = "debug", skip_all)]
    pub async fn history(
        &self,
        view: ViewId,
        before: Option<u64>,
        limit: NonZeroUsize,
    ) -> Result<HistoryPage, BrowseError> {
        let record = self.view(view).await?;
        let size = record.order.size;
        let before = before.unwrap_or(size);
        if before > size {
            return Err(BrowseError::PositionOutOfRange {
                position: before,
                size,
            });
        }
        let Some(first) = before.checked_sub(1) else {
            return Ok(HistoryPage::default());
        };
        let Some((mut id, mut application)) = self.reader.application_at(&record, first).await?
        else {
            return Err(BrowseError::IdentityMismatch("application order"));
        };
        let mut items = Vec::with_capacity(limit.get().min(1024));
        loop {
            if application.order.position != first - items.len() as u64 {
                return Err(BrowseError::IdentityMismatch("application"));
            }
            let previous = application.order.previous;
            let item = self.item(id, application).await?;
            let position = item.position;
            items.push(item);
            if position == 0 || items.len() == limit.get() {
                return Ok(HistoryPage {
                    items,
                    next: (position > 0).then_some(position),
                });
            }
            id = previous.ok_or(BrowseError::IdentityMismatch("application order"))?;
            application = self.reader.application(id).await?;
        }
    }

    /// Up to `limit` of `view`'s changes before canonical position `before` (all of
    /// them when `None`) that touch `path`, newest first: for a file the changes that
    /// changed it, for a directory those that changed anything below it, for `""`
    /// every change. `index` answers which changes those are (see
    /// [`PathHistoryIndex::touching`]), so a page costs what its changes number, not
    /// the changes between them.
    #[tracing::instrument(name = "browse_path_history", level = "debug", skip_all)]
    pub async fn path_history(
        &self,
        index: &PathHistoryIndex,
        view: ViewId,
        path: &str,
        before: Option<u64>,
        limit: NonZeroUsize,
    ) -> Result<HistoryPage, BrowseError> {
        if components(path)?.is_empty() {
            return self.history(view, before, limit).await;
        }
        let record = self.view(view).await?;
        let size = record.order.size;
        if let Some(position) = before.filter(|&position| position > size) {
            return Err(BrowseError::PositionOutOfRange { position, size });
        }
        let touches = index
            .touching(&self.reader, &record, path, before, limit)
            .await?;
        let mut items = Vec::with_capacity(touches.touches.len());
        for touch in touches.touches {
            let application = self.reader.application(touch.application).await?;
            if application.order.position != touch.position {
                return Err(BrowseError::IdentityMismatch("application"));
            }
            items.push(self.item(touch.application, application).await?);
        }
        Ok(HistoryPage {
            items,
            next: touches.next,
        })
    }

    /// Which change last wrote each line of the file at `path` in `view`, or `None`
    /// when no file is there; see [`crate::blame()`]. `index` finds the file's
    /// changes, so the walk costs what they number, up to `limits.max_changes`. Each
    /// change a range names costs a history item's reads more.
    #[tracing::instrument(name = "browse_blame", level = "debug", skip_all)]
    pub async fn blame(
        &self,
        index: &PathHistoryIndex,
        view: ViewId,
        path: &str,
        limits: BlameLimits,
    ) -> Result<Option<Blame<HistoryItem>>, BrowseError> {
        let record = self.view(view).await?;
        let Some(blame) = crate::blame(&self.reader, index, &record, path, limits).await? else {
            return Ok(None);
        };
        let mut origins = Vec::with_capacity(blame.origins.len());
        for origin in blame.origins {
            origins.push(
                self.item(origin.touch.application, origin.application)
                    .await?,
            );
        }
        Ok(Some(Blame {
            lines: blame.lines,
            ranges: blame.ranges,
            origins,
        }))
    }

    /// Up to `limit` of `view`'s changes by when their revisions were committed, newest
    /// first, from `from` on (from the newest when `None`). Finding the first costs
    /// O(log n) node reads in the view's history map and each change a few object
    /// reads, so a page does not read the whole history.
    #[tracing::instrument(name = "browse_history_by_time", level = "debug", skip_all)]
    pub async fn history_by_time(
        &self,
        view: ViewId,
        from: Option<HistoryKey>,
        limit: NonZeroUsize,
    ) -> Result<TimeHistoryPage, BrowseError> {
        let record = self.view(view).await?;
        let history = HistoryMapStore::new(self.reader.objects);
        let mut cursor = match from {
            Some(from) => history.cursor_from(record.history, from).await?,
            None => history.cursor(record.history).await?,
        };
        let mut items = Vec::with_capacity(limit.get().min(1024));
        while let Some((key, id)) = cursor.next().await? {
            if items.len() == limit.get() {
                return Ok(TimeHistoryPage {
                    items,
                    next: Some(key),
                });
            }
            let application = self.reader.application(id).await?;
            let item = self.item(id, application).await?;
            if item.time_key() != key {
                return Err(BrowseError::IdentityMismatch("history"));
            }
            items.push(item);
        }
        Ok(TimeHistoryPage { items, next: None })
    }

    /// The history item for application `id`: reads its revision and change.
    async fn item(
        &self,
        id: ApplicationId,
        application: ApplicationRecord,
    ) -> Result<HistoryItem, BrowseError> {
        if application.id() != id {
            return Err(BrowseError::IdentityMismatch("application"));
        }
        let revision = self.reader.revision(application.revision_id).await?;
        if revision.id() != application.revision_id {
            return Err(BrowseError::IdentityMismatch("revision"));
        }
        let change_object = ChangeMapStore::new(self.reader.objects)
            .get(self.state.changes, revision.change_id)
            .await
            .map_err(RepositoryPublicationError::from)?
            .ok_or(BrowseError::IdentityMismatch("change map"))?;
        let change = self.reader.change(change_object).await?;
        if change.id != revision.change_id {
            return Err(BrowseError::IdentityMismatch("change"));
        }
        let (tree, conflicted) = match application.result {
            ApplicationResultRecord::Clean { tree } => (tree, false),
            ApplicationResultRecord::Conflicted { partial_tree, .. } => (partial_tree, true),
        };
        Ok(HistoryItem {
            position: application.order.position,
            application: id,
            parent: application.target_view,
            tree,
            conflicted,
            change: revision.change_id,
            title: change.metadata.title,
            description: change.metadata.description,
            creator: change.metadata.creator,
            revision: application.revision_id,
            message: revision.metadata.message,
            author: revision.metadata.author,
            authored_at: revision.metadata.authored_at,
            committer: revision.metadata.committer,
            committed_at: revision.metadata.committed_at,
        })
    }
}

/// `path`'s components: none for `""`, otherwise split at `/`.
fn components(path: &str) -> Result<Vec<String>, BrowseError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    path.split('/')
        .map(|component| {
            if component.is_empty() {
                Err(BrowseError::InvalidPath(path.to_owned()))
            } else {
                Ok(component.to_owned())
            }
        })
        .collect()
}
