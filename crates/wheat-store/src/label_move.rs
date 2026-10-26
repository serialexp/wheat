//! Whether a label may move from one view to another.
//!
//! A move from `O` to `N` is forward when integrating `O` into `N` would change
//! nothing: for every change `O` holds at revision `o`, `N` holds it at some `n`, and
//! merging `o` into `n` (see [`crate::revision_merge`]) gives exactly `n`'s edits. So a
//! descendant is forward from each of its ancestors, a revision with the same edits on
//! another line is forward, and so is a later revision of that line that removed one of
//! them. `N` may hold changes `O` lacks.
//!
//! The work follows the changes whose revisions differ, through the revision maps'
//! diff, and for each the revisions between the two and where they meet, never the
//! views' whole history. Every check of one command or publish shares one
//! [`MergeBudget`].

use wheat_core::{ChangeId, RepositoryEnvelopeId, ViewId, ViewRecord};

use crate::{
    ImmutableObjectStore, MergeBudget, RepositoryPublicationError, RepositoryReader,
    RevisionMapDelta, RevisionMapDeltaLimits, RevisionMapStore, revision_merge::RevisionMerge,
};

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

/// Why a move is not forward: the first change, in change id order, it would lose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotForward {
    /// The target lacks a change the source holds.
    Missing { change: ChangeId },
    /// Merging the source's revision into the target's would add edits the target
    /// lacks, or drop edits it holds.
    Diverged {
        change: ChangeId,
        edits_added: usize,
        edits_dropped: usize,
    },
}

/// A label one head moved to a view that is not forward from where it was.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackwardLabel {
    pub name: String,
    pub from: ViewId,
    pub to: ViewId,
    pub not_forward: NotForward,
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// The first label, by name, that `new` names a different view than `old` does
    /// and that is not forward from it. Labels `new` creates or deletes are not moves.
    /// Every check shares `budget`.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn backward_label(
        &self,
        old: RepositoryEnvelopeId,
        new: RepositoryEnvelopeId,
        budget: &mut MergeBudget,
    ) -> Result<Option<BackwardLabel>> {
        let (old, new) = (self.envelope(old).await?, self.envelope(new).await?);
        if old.labels == new.labels {
            return Ok(None);
        }
        let old = self.view_label_manifest(old.labels).await?;
        let new = self.view_label_manifest(new.labels).await?;
        for (name, label) in &new.labels {
            let Some(was) = old.labels.get(name).filter(|was| was.view != label.view) else {
                continue;
            };
            let (from, to) = (self.view(was.view).await?, self.view(label.view).await?);
            if let Some(not_forward) = self.forward(&from, &to, budget).await? {
                return Ok(Some(BackwardLabel {
                    name: name.clone(),
                    from: was.view,
                    to: label.view,
                    not_forward,
                }));
            }
        }
        Ok(None)
    }

    /// Whether a label naming `from` may move to `to`; what is left of `budget` after
    /// the merges it takes stays in it for the command's next check.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn forward(
        &self,
        from: &ViewRecord,
        to: &ViewRecord,
        budget: &mut MergeBudget,
    ) -> Result<Option<NotForward>> {
        let delta = RevisionMapStore::new(self.objects)
            .delta(
                from.revisions,
                to.revisions,
                RevisionMapDeltaLimits::default(),
            )
            .await?;
        for entry in delta {
            match entry {
                RevisionMapDelta::Added { .. } => {}
                RevisionMapDelta::Removed { change, .. } => {
                    return Ok(Some(NotForward::Missing { change }));
                }
                RevisionMapDelta::Changed {
                    change,
                    basis,
                    frontier,
                } => {
                    let mut merge = RevisionMerge::new(self, change, *budget);
                    let merged = merge.merged_edits(&[basis, frontier]).await?;
                    budget.spend(merge.work());
                    let edits = &merge.record(frontier).await?.edits;
                    if merged != *edits {
                        return Ok(Some(NotForward::Diverged {
                            change,
                            edits_added: merged.difference(edits).count(),
                            edits_dropped: edits.difference(&merged).count(),
                        }));
                    }
                }
            }
        }
        Ok(None)
    }
}
