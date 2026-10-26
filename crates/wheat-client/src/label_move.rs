//! The checks a label move passes before Wheat makes it: the move is forward, and it
//! leaves no frontier of the label's old view behind (see
//! `docs/design/wheat-label-moves.md`). `label move` runs them, and so do the commands
//! that move the active label as part of their work.

use anyhow::Result;
use wheat_core::{RepositoryStateRecord, ViewId};
use wheat_store::{
    ImmutableObjectStore, MergeBudget, NotForward, RepositoryReader, ViewLineageLimits,
    ViewLineageMapStore,
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
    let mut refusal = MoveRefusal::default();
    if from == to {
        return Ok(refusal);
    }
    let target = reader.retained_view(state, to).await?;
    let source = reader.retained_view(state, from).await?;
    refusal.not_forward = reader.forward(&source, &target, budget).await?;
    let mut frontiers = ViewLineageMapStore::new(objects)
        .descendant_frontiers(state.view_lineage, from, ViewLineageLimits::default())
        .await?;
    if frontiers.is_empty() {
        // `from` is its own only frontier, and the forward check above covers it.
        return Ok(refusal);
    }
    frontiers.sort();
    for frontier in frontiers {
        if frontier == to {
            continue;
        }
        let view = reader.view(frontier).await?;
        if reader.forward(&view, &target, budget).await?.is_some() {
            refusal.left_behind.push(frontier);
        }
    }
    Ok(refusal)
}
