//! The checks a label move passes before Wheat makes it: the move is forward, and it
//! leaves no frontier of the label's old view behind (see
//! `docs/design/wheat-label-moves.md`). `label move` runs them, and so do the commands
//! that move the active label as part of their work.

use anyhow::Result;
use std::collections::BTreeSet;

use wheat_core::{RepositoryStateRecord, ViewId, ViewRecord};
use wheat_store::{
    ImmutableObjectStore, LabelChange, MergeBudget, NotForward, RepositoryReader,
    ViewLineageLimits, ViewLineageMapStore,
};

/// Why a move from one view to another may not happen without being forced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MoveRefusal {
    /// The first change the move would lose, when it is not forward.
    pub not_forward: Option<NotForward>,
    /// Frontiers of the old view the new view does not cover, in view order.
    pub left_behind: Vec<ViewId>,
}

impl MoveRefusal {
    pub fn is_empty(&self) -> bool {
        self.not_forward.is_none() && self.left_behind.is_empty()
    }

    /// One sentence for each reason, naming label `name` and the target view `to`.
    pub fn describe(&self, name: &str, to: ViewId) -> String {
        let mut reasons = Vec::new();
        match self.not_forward {
            Some(NotForward::Missing { change }) => reasons.push(format!(
                "view {to} lacks change {change}, which label {name} holds"
            )),
            Some(NotForward::Diverged {
                change,
                edits_added,
                edits_dropped,
            }) => reasons.push(format!(
                "label {name}'s revision of change {change} is not in view {to}: merging it \
                 would add {edits_added} and drop {edits_dropped} edits"
            )),
            None => {}
        }
        if !self.left_behind.is_empty() {
            let views = self
                .left_behind
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!(
                "view {to} does not cover the frontiers {views} of label {name}, which would \
                 be left behind; integrate them first"
            ));
        }
        reasons.join("; ")
    }
}

/// Checks moving a label from `from` to `to` in `state`, sharing `budget` with the
/// command's other checks. Left behind are the descendant frontiers of `from`, or
/// `from` itself when it has none, that are not forward to `to`.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn check<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    objects: &O,
    state: &RepositoryStateRecord,
    from: ViewId,
    to: ViewId,
    budget: &mut MergeBudget,
) -> Result<MoveRefusal> {
    if from == to {
        return Ok(MoveRefusal::default());
    }
    let target = reader.retained_view(state, to).await?;
    let source = reader.retained_view(state, from).await?;
    let frontiers = ViewLineageMapStore::new(objects)
        .descendant_frontiers(state.view_lineage, from, ViewLineageLimits::default())
        .await?;
    check_target(reader, &source, &target, to, frontiers, budget).await
}

/// [`check`] against a target the head may not retain yet, such as the view an
/// integration is about to publish, read through `reader` from where it is staged.
/// `frontiers` are the descendant frontiers of the label's view in the state before
/// the move.
pub async fn check_target<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    source: &ViewRecord,
    target: &ViewRecord,
    to: ViewId,
    mut frontiers: Vec<ViewId>,
    budget: &mut MergeBudget,
) -> Result<MoveRefusal> {
    let mut refusal = MoveRefusal {
        not_forward: reader.forward(source, target, budget).await?,
        left_behind: Vec::new(),
    };
    // Without frontiers the label's view is its own only one, and the forward check
    // above covers it.
    frontiers.sort();
    for frontier in frontiers {
        if frontier == to {
            continue;
        }
        let view = reader.view(frontier).await?;
        if reader.forward(&view, target, budget).await?.is_some() {
            refusal.left_behind.push(frontier);
        }
    }
    Ok(refusal)
}
/// A label a fetch would change, and the views the change would hide.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HiddenByFetch {
    pub label: String,
    pub views: Vec<ViewId>,
}

/// The views a fetch's label changes would hide that no workspace in `shown` shows.
/// A moved label hides what the move leaves behind ([`check_target`]: the local view's
/// frontiers the new view does not cover, or the local view itself when it has none
/// and is not covered). A deleted label hides its local view's descendant frontiers,
/// the local work built on it; its own view is the remote's to drop. `state` is the
/// local state before the fetch; `reader` also reads what the fetch staged.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn hidden_by_fetch<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    objects: &O,
    state: &RepositoryStateRecord,
    changes: &[LabelChange],
    shown: &BTreeSet<ViewId>,
    budget: &mut MergeBudget,
) -> Result<Vec<HiddenByFetch>> {
    let mut hidden = Vec::new();
    for change in changes {
        let Some(before) = &change.before else {
            continue;
        };
        let frontiers = ViewLineageMapStore::new(objects)
            .descendant_frontiers(
                state.view_lineage,
                before.view,
                ViewLineageLimits::default(),
            )
            .await?;
        let mut views = match &change.after {
            Some(after) if after.view == before.view => continue,
            Some(after) => {
                let source = reader.view(before.view).await?;
                let target = reader.view(after.view).await?;
                let has_frontiers = !frontiers.is_empty();
                let refusal =
                    check_target(reader, &source, &target, after.view, frontiers, budget).await?;
                let mut views = refusal.left_behind;
                if !has_frontiers && refusal.not_forward.is_some() {
                    views.push(before.view);
                }
                views
            }
            None => frontiers,
        };
        views.retain(|view| !shown.contains(view));
        if !views.is_empty() {
            views.sort();
            hidden.push(HiddenByFetch {
                label: change.name.clone(),
                views,
            });
        }
    }
    Ok(hidden)
}
