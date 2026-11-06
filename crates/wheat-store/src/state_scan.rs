//! Everything a repository head's state retains that holds file content, a page at a
//! time, for a host scanning all a clone would serve (Gothab's secret sweep): every
//! tree root with what it belongs to, and the content of every edit and conflict with
//! its path. Superseded revisions, unapplied changes and conflicts are included, since
//! a clone receives them too.
//!
//! Both walks are in a fixed order (by section, then by ID), so a scan stopped early
//! is the same scan each time, and a page's cursor resumes the walk at any later call
//! on the same state. A page reads its own records plus O(log n) set nodes to find
//! its start, however deep in the walk it is.

use std::num::NonZeroUsize;

use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, ContentId, EditId, EditOperation, ObjectId,
    RetainedSetId, RevisionId, TreeId, ViewId,
};

use crate::{
    BrowseError, ImmutableObjectStore, RepositoryBrowser, RepositoryPublicationError,
    RetainedSetStore,
};

/// What a tree root from [`RepositoryBrowser::state_trees`] belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeOrigin {
    /// A retained view's tree.
    View(ViewId),
    /// A revision's resulting tree.
    Revision {
        revision: RevisionId,
        change: ChangeId,
    },
    /// An application's tree: the materialized one, or the partial one when it
    /// conflicted.
    Application {
        application: ApplicationId,
        revision: RevisionId,
        /// The view it applies onto.
        target_view: ViewId,
        conflicted: bool,
    },
}

/// One tree root a state retains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateTree {
    pub tree: TreeId,
    pub origin: TreeOrigin,
}

/// Where [`RepositoryBrowser::state_trees`] resumes: after the record named. The
/// walk goes through views, then revisions, then applications, each by ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateTreeCursor {
    AfterView(ViewId),
    AfterRevision(RevisionId),
    AfterApplication(ApplicationId),
}

/// A page of [`StateTree`]s.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateTreePage {
    pub trees: Vec<StateTree>,
    /// Where the next page starts; `None` once every tree is listed.
    pub next: Option<StateTreeCursor>,
}

/// What an edit from [`RepositoryBrowser::state_edits`] belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditOrigin {
    /// One of a revision's edits.
    Revision(RevisionId),
    /// An edit a conflicted application failed, with the file it found.
    Conflict {
        application: ApplicationId,
        revision: RevisionId,
    },
}

/// The content one edit names, at its path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateEdit {
    pub origin: EditOrigin,
    pub edit: EditId,
    pub path: String,
    /// For a revision's edit: an exact file edit's expected and replacement content,
    /// or a guarded text edit's before, expected, replacement and after fragments.
    /// For a conflict: the content the edit found at its path, if any. Repeats are
    /// kept, in that order.
    pub contents: Vec<ContentId>,
}

/// Where [`RepositoryBrowser::state_edits`] resumes. The walk goes through every
/// revision's edits, then every conflicted application's failed edits, each by
/// revision or application ID and then by edit ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateEditCursor {
    /// At `revision`, after its edit `after`.
    InRevision { revision: RevisionId, after: EditId },
    /// After every edit of `revision`.
    AfterRevision(RevisionId),
    /// At `application`'s conflict, after its edit `after`.
    InConflict {
        application: ApplicationId,
        after: EditId,
    },
    /// After `application`, conflicted or not.
    AfterApplication(ApplicationId),
}

/// A page of [`StateEdit`]s.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateEditPage {
    /// At most the page's limit. A page can hold fewer, even none, before the walk
    /// ends: each also visits at most `limit` revisions or applications, so a run of
    /// clean applications costs a bounded number of reads a page.
    pub edits: Vec<StateEdit>,
    /// Where the next page starts; `None` once every edit is listed.
    pub next: Option<StateEditCursor>,
}

/// Where a walk starts in a retained set.
#[derive(Clone, Copy)]
enum Start {
    First,
    At(ObjectId),
    After(ObjectId),
}

/// A retained set's IDs in order, from a [`Start`].
struct Retained<'a, O> {
    cursor: crate::RetainedSetCursor<'a, O>,
    /// The first ID, read to see whether it was the `Start::After` bound.
    pending: Option<ObjectId>,
}

impl<'a, O: ImmutableObjectStore> Retained<'a, O> {
    async fn open(objects: &'a O, root: RetainedSetId, start: Start) -> Result<Self, BrowseError> {
        let store = RetainedSetStore::new(objects);
        let cursor = match start {
            Start::First => store.cursor(root).await,
            Start::At(id) | Start::After(id) => store.cursor_from(root, id).await,
        }
        .map_err(RepositoryPublicationError::from)?;
        let mut retained = Self {
            cursor,
            pending: None,
        };
        if let Start::After(bound) = start {
            // The cursor starts at the bound itself when the set holds it.
            retained.pending = retained.next().await?.filter(|first| *first != bound);
        }
        Ok(retained)
    }

    async fn next(&mut self) -> Result<Option<ObjectId>, BrowseError> {
        if let Some(id) = self.pending.take() {
            return Ok(Some(id));
        }
        Ok(self
            .cursor
            .next()
            .await
            .map_err(RepositoryPublicationError::from)?)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TreeSection {
    Views,
    Revisions,
    Applications,
}

impl<O: ImmutableObjectStore> RepositoryBrowser<'_, O> {
    /// Up to `limit` of the tree roots the state retains, from `from` on (`None` for
    /// the first page). Each tree read is one object: a view, revision or application
    /// record. Trees repeat across origins (a view's tree is usually its last
    /// application's), so a caller walking them should skip subtrees it has seen.
    pub async fn state_trees(
        &self,
        from: Option<StateTreeCursor>,
        limit: NonZeroUsize,
    ) -> Result<StateTreePage, BrowseError> {
        let (mut section, mut after) = match from {
            None => (TreeSection::Views, None),
            Some(StateTreeCursor::AfterView(id)) => (TreeSection::Views, Some(id.into())),
            Some(StateTreeCursor::AfterRevision(id)) => (TreeSection::Revisions, Some(id.into())),
            Some(StateTreeCursor::AfterApplication(id)) => {
                (TreeSection::Applications, Some(id.into()))
            }
        };
        let objects = self.reader.objects;
        let mut trees = Vec::with_capacity(limit.get().min(1024));
        loop {
            let root = match section {
                TreeSection::Views => self.state.retained_views,
                TreeSection::Revisions => self.state.retained_revisions,
                TreeSection::Applications => self.state.retained_applications,
            };
            let start = after.take().map_or(Start::First, Start::After);
            let mut cursor = Retained::open(objects, root, start).await?;
            while let Some(id) = cursor.next().await? {
                let digest = *id.as_bytes();
                let (tree, next) = match section {
                    TreeSection::Views => {
                        let view = ViewId::from_digest(digest);
                        let record = self.reader.view(view).await?;
                        (
                            StateTree {
                                tree: record.tree,
                                origin: TreeOrigin::View(view),
                            },
                            StateTreeCursor::AfterView(view),
                        )
                    }
                    TreeSection::Revisions => {
                        let revision = RevisionId::from_digest(digest);
                        let record = self.reader.revision(revision).await?;
                        (
                            StateTree {
                                tree: record.resulting_tree,
                                origin: TreeOrigin::Revision {
                                    revision,
                                    change: record.change_id,
                                },
                            },
                            StateTreeCursor::AfterRevision(revision),
                        )
                    }
                    TreeSection::Applications => {
                        let application = ApplicationId::from_digest(digest);
                        let record = self.reader.application(application).await?;
                        let (tree, conflicted) = match record.result {
                            ApplicationResultRecord::Clean { tree } => (tree, false),
                            ApplicationResultRecord::Conflicted { partial_tree, .. } => {
                                (partial_tree, true)
                            }
                        };
                        (
                            StateTree {
                                tree,
                                origin: TreeOrigin::Application {
                                    application,
                                    revision: record.revision_id,
                                    target_view: record.target_view,
                                    conflicted,
                                },
                            },
                            StateTreeCursor::AfterApplication(application),
                        )
                    }
                };
                trees.push(tree);
                if trees.len() == limit.get() {
                    let next = self.tree_after(section, next, cursor).await?;
                    return Ok(StateTreePage { trees, next });
                }
            }
            section = match section {
                TreeSection::Views => TreeSection::Revisions,
                TreeSection::Revisions => TreeSection::Applications,
                TreeSection::Applications => return Ok(StateTreePage { trees, next: None }),
            };
        }
    }

    /// `next`, unless nothing follows it: a full last page says the walk is over.
    async fn tree_after(
        &self,
        section: TreeSection,
        next: StateTreeCursor,
        mut cursor: Retained<'_, O>,
    ) -> Result<Option<StateTreeCursor>, BrowseError> {
        if cursor.next().await?.is_some() {
            return Ok(Some(next));
        }
        let later = [
            (TreeSection::Revisions, self.state.retained_revisions),
            (TreeSection::Applications, self.state.retained_applications),
        ];
        for (later, root) in later {
            if later > section
                && Retained::open(self.reader.objects, root, Start::First)
                    .await?
                    .next()
                    .await?
                    .is_some()
            {
                return Ok(Some(next));
            }
        }
        Ok(None)
    }

    /// Up to `limit` edits and the content they name, from `from` on (`None` for the
    /// first page); see [`StateEditPage`] for why a page can be short. A page reads
    /// each revision or application it visits, a conflict for each conflicted one,
    /// and each edit it lists.
    pub async fn state_edits(
        &self,
        from: Option<StateEditCursor>,
        limit: NonZeroUsize,
    ) -> Result<StateEditPage, BrowseError> {
        let limit = limit.get();
        let objects = self.reader.objects;
        // The section, where in it to start, and the edits to skip at that start.
        let (mut section, mut start, mut skip) = match from {
            None => (EditSection::Revisions, Start::First, None),
            Some(StateEditCursor::InRevision { revision, after }) => (
                EditSection::Revisions,
                Start::At(revision.into()),
                Some(after),
            ),
            Some(StateEditCursor::AfterRevision(revision)) => {
                (EditSection::Revisions, Start::After(revision.into()), None)
            }
            Some(StateEditCursor::InConflict { application, after }) => (
                EditSection::Conflicts,
                Start::At(application.into()),
                Some(after),
            ),
            Some(StateEditCursor::AfterApplication(application)) => (
                EditSection::Conflicts,
                Start::After(application.into()),
                None,
            ),
        };
        let mut edits = Vec::with_capacity(limit.min(1024));
        let mut visited = 0;
        loop {
            let root = match section {
                EditSection::Revisions => self.state.retained_revisions,
                EditSection::Conflicts => self.state.retained_applications,
            };
            let mut cursor = Retained::open(objects, root, start).await?;
            let mut skip_at = match start {
                Start::At(id) => skip.take().map(|after| (id, after)),
                _ => None,
            };
            start = Start::First;
            while let Some(id) = cursor.next().await? {
                visited += 1;
                let after = skip_at
                    .take()
                    .filter(|(at, _)| *at == id)
                    .map(|(_, after)| after);
                let digest = *id.as_bytes();
                // The owner's edits in ID order: for a conflict, each with the
                // content it found.
                let (origin, owned): (EditOrigin, Vec<(EditId, Option<ContentId>)>) = match section
                {
                    EditSection::Revisions => {
                        let revision = RevisionId::from_digest(digest);
                        let record = self.reader.revision(revision).await?;
                        let owned = record
                            .edits
                            .iter()
                            .filter(|edit| after.is_none_or(|after| **edit > after))
                            .map(|edit| (*edit, None))
                            .collect();
                        (EditOrigin::Revision(revision), owned)
                    }
                    EditSection::Conflicts => {
                        let application = ApplicationId::from_digest(digest);
                        let record = self.reader.application(application).await?;
                        let owned = match record.result {
                            ApplicationResultRecord::Clean { .. } => Vec::new(),
                            ApplicationResultRecord::Conflicted { conflict, .. } => self
                                .reader
                                .conflict(conflict)
                                .await?
                                .edits
                                .into_iter()
                                .filter(|(edit, _)| after.is_none_or(|after| *edit > after))
                                .map(|(edit, evidence)| {
                                    (edit, evidence.target.map(|target| target.content))
                                })
                                .collect(),
                        };
                        let origin = EditOrigin::Conflict {
                            application,
                            revision: record.revision_id,
                        };
                        (origin, owned)
                    }
                };
                let count = owned.len();
                for (index, (edit, found)) in owned.into_iter().enumerate() {
                    let record = self.reader.edit(edit).await?;
                    let contents = match origin {
                        EditOrigin::Revision(_) => operation_contents(&record.operation),
                        EditOrigin::Conflict { .. } => found.into_iter().collect(),
                    };
                    edits.push(StateEdit {
                        origin,
                        edit,
                        path: record.path,
                        contents,
                    });
                    if edits.len() == limit && index + 1 < count {
                        return Ok(StateEditPage {
                            edits,
                            next: Some(origin.within(edit)),
                        });
                    }
                }
                if edits.len() == limit || visited == limit {
                    return Ok(StateEditPage {
                        edits,
                        next: Some(origin.after()),
                    });
                }
            }
            section = match section {
                EditSection::Revisions => EditSection::Conflicts,
                EditSection::Conflicts => return Ok(StateEditPage { edits, next: None }),
            };
        }
    }
}

#[derive(Clone, Copy)]
enum EditSection {
    Revisions,
    Conflicts,
}

impl EditOrigin {
    /// A cursor resuming at this owner, after `edit`.
    fn within(self, edit: EditId) -> StateEditCursor {
        match self {
            Self::Revision(revision) => StateEditCursor::InRevision {
                revision,
                after: edit,
            },
            Self::Conflict { application, .. } => StateEditCursor::InConflict {
                application,
                after: edit,
            },
        }
    }

    /// A cursor resuming after this owner.
    fn after(self) -> StateEditCursor {
        match self {
            Self::Revision(revision) => StateEditCursor::AfterRevision(revision),
            Self::Conflict { application, .. } => StateEditCursor::AfterApplication(application),
        }
    }
}

/// The content an edit's operation names, in the order [`StateEdit::contents`] gives.
fn operation_contents(operation: &EditOperation) -> Vec<ContentId> {
    match operation {
        EditOperation::GuardedText {
            before,
            expected,
            replacement,
            after,
            ..
        } => vec![*before, *expected, *replacement, *after],
        EditOperation::ExactFile {
            expected,
            replacement,
        } => expected
            .iter()
            .chain(replacement)
            .map(|value| value.content)
            .collect(),
    }
}
