//! Reads for browsing a repository without a workspace, as a host's repository pages
//! do: a label's view, a directory's entries, a file's bytes, and a view's history,
//! newest first, a page at a time. Every read is bounded by its page or byte limit
//! rather than by the repository's size.

use std::num::NonZeroUsize;

use thiserror::Error;
use wheat_core::{
    Actor, ApplicationId, ApplicationResultRecord, ChangeId, FileValue, RepositoryStateRecord,
    RevisionId, Timestamp, TreeId, ViewId, ViewLabelManifestRecord, ViewRecord,
};

use crate::{
    ChangeMapStore, ContentStore, ContentStoreError, ImmutableObjectStore, MerkleTreeError,
    MerkleTreeStore, RepositoryHead, RepositoryPublicationError, RepositoryReader,
    RetainedSetStore, TreeListing,
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
    Content(#[from] ContentStoreError),
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
    pub creator: Actor,
    pub revision: RevisionId,
    pub message: String,
    pub author: Actor,
    pub committed_at: Timestamp,
}

/// A page of history, newest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    /// The `before` for the next page; none after the oldest change.
    pub next: Option<u64>,
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
        let changes = ChangeMapStore::new(self.reader.objects);
        let mut items = Vec::with_capacity(limit.get().min(1024));
        loop {
            if application.id() != id || application.order.position != first - items.len() as u64 {
                return Err(BrowseError::IdentityMismatch("application"));
            }
            let revision = self.reader.revision(application.revision_id).await?;
            if revision.id() != application.revision_id {
                return Err(BrowseError::IdentityMismatch("revision"));
            }
            let change_object = changes
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
            items.push(HistoryItem {
                position: application.order.position,
                application: id,
                parent: application.target_view,
                tree,
                conflicted,
                change: revision.change_id,
                title: change.metadata.title,
                creator: change.metadata.creator,
                revision: application.revision_id,
                message: revision.metadata.message,
                author: revision.metadata.author,
                committed_at: revision.metadata.committed_at,
            });
            let position = application.order.position;
            if position == 0 || items.len() == limit.get() {
                return Ok(HistoryPage {
                    items,
                    next: (position > 0).then_some(position),
                });
            }
            id = application
                .order
                .previous
                .ok_or(BrowseError::IdentityMismatch("application order"))?;
            application = self.reader.application(id).await?;
        }
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
