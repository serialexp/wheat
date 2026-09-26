//! Stacking many checkpoints.
//!
//! Revisions are published one at a time, the way repeated `wheat add` calls do, and
//! the top change's revisions are timed. The stack depth stays fixed while history
//! grows, so any growth in a revision's cost comes from the history behind it:
//!
//! - `single`: one change gets `checkpoints` revisions;
//! - `deep`: the same on top of a stack `depth` changes deep;
//! - `spread`: the same stack, but every lower change first gets `per_change`
//!   revisions of its own (the history of a stack edited change by change), then the
//!   top change gets `window` revisions. Its reference is `deep`'s, at the same depth.
//!
//! Each shape records the median of a reference window of `window` revisions, starting
//! a quarter of the way in, and of its last `window` revisions; the history behind the
//! last window is four times as long, and the difference must be at most
//! [`ADDED_BUDGET_MS`]. The reference skips the first few hundred revisions, which get
//! slower while the new store warms up and then level off (measured 2026-09-26: 1.5 ms
//! rising to 2.6 ms by revision 400, then flat to 2,000). The stores defer their syncs,
//! so this measures the mutation's own work rather than the disk.

use std::{collections::BTreeSet, path::Path, time::Instant};

use anyhow::{Context, Result, ensure};
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, EditId, RevisionId, RevisionMetadata, ViewId,
};
use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PersistedRepositoryMutator, RepositoryHead,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Scratch, author, rewrite},
};
use crate::measure;

pub(crate) const NAME: &str = "checkpoint_stack";
/// Allowed extra cost of a late revision over the reference at the same depth. Before
/// mutations proved ancestry along the composition path, a single change grew from
/// 1.4 ms to 5.2 ms over 2,000 revisions and the spread top revision cost 86 ms.
const ADDED_BUDGET_MS: f64 = 1.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(2_000, 60);
    let window = context.scale.pick(100, 10);
    let depth = context.scale.pick(50, 5);
    let per_change = context.scale.pick(40, 12);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", checkpoints);
    recorder.parameter("window", window);
    recorder.parameter("depth", depth);
    recorder.parameter("per_change", per_change);
    ensure!(
        checkpoints / 4 + window <= checkpoints - window,
        "the reference and last windows must not overlap"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut shape = |name: &str, depth: usize, per_change: usize, top: usize| {
        let started = Instant::now();
        let root = context.scratch.join(name);
        let revisions = runtime.block_on(stack(&root, depth, per_change, top))?;
        recorder.wall(&format!("{name}.build"), started.elapsed().as_secs_f64());
        let per_revision = |part: &[f64]| measure::median(part.to_vec()) * 1_000.0;
        let reference = checkpoints / 4;
        let reference = revisions
            .get(reference..reference + window)
            .map(per_revision);
        let late = per_revision(&revisions[revisions.len() - window..]);
        anyhow::Ok((reference, late))
    };

    let (single_reference, single_late) = shape("single", 1, 1, checkpoints)?;
    let (deep_reference, deep_late) = shape("deep", depth, 1, checkpoints)?;
    let (_, spread_late) = shape("spread", depth, per_change, window)?;
    let reference = |value: Option<f64>| value.context("the reference window must fit");
    let deep_reference = reference(deep_reference)?;
    for (name, reference, late) in [
        ("single", reference(single_reference)?, single_late),
        ("deep", deep_reference, deep_late),
        ("spread", deep_reference, spread_late),
    ] {
        recorder.value(&format!("{name}.reference.per_revision"), reference, "ms");
        recorder.value(&format!("{name}.late.per_revision"), late, "ms");
        let added = format!("{name}.added.per_revision");
        recorder.value(&added, late - reference, "ms");
        recorder.at_most(&added, ADDED_BUDGET_MS, "ms");
    }
    Ok(recorder.finish())
}

type Mutator<'a> = PersistedRepositoryMutator<'a, PackedObjectStore, LocalPublicationStore>;

/// A change's latest revision and the edit it wrote to the change's file.
#[derive(Clone, Copy)]
struct Latest {
    revision: RevisionId,
    edit: EditId,
}

/// Where the next revision goes.
struct Tip {
    base: ViewId,
    effective: ViewId,
    head: RepositoryHead,
}

/// Builds `depth` changes, each created on top and then revised until it has
/// `per_change` revisions, except the top one, which is revised `top` more times.
/// Returns the wall time of each of those `top` revisions in seconds, from preparing
/// it to publishing it.
async fn stack(root: &Path, depth: usize, per_change: usize, top: usize) -> Result<Vec<f64>> {
    let scratch = Scratch::open(root)?;
    let (base, head) = scratch.initialize().await?;
    let mutator = scratch.mutator();
    let mut tip = Tip {
        base,
        effective: base,
        head,
    };
    let mut below = None;
    let mut timed = Vec::with_capacity(top);
    for index in 0..depth {
        let change = ChangeId::new();
        let path = format!("src/f{index:03}.txt");
        let mut latest = create(&scratch, &mutator, &mut tip, change, below, &path, index).await?;
        let revisions = if index + 1 == depth {
            top
        } else {
            per_change - 1
        };
        for checkpoint in 0..revisions {
            let started = Instant::now();
            latest = revise(
                &scratch, &mutator, &mut tip, change, &path, latest, checkpoint,
            )
            .await?;
            if index + 1 == depth {
                timed.push(started.elapsed().as_secs_f64());
            }
        }
        below = Some(change);
    }
    Ok(timed)
}

async fn create(
    scratch: &Scratch,
    mutator: &Mutator<'_>,
    tip: &mut Tip,
    change: ChangeId,
    below: Option<ChangeId>,
    path: &str,
    index: usize,
) -> Result<Latest> {
    let (edit, edits) = rewrite(&scratch.content, path, b"created\n", None)?;
    let (prepared, outcome) = mutator
        .create(
            tip.head,
            tip.base,
            tip.effective,
            ChangeProposal {
                id: change,
                metadata: ChangeMetadata {
                    title: format!("Change {index}"),
                    creator: author(),
                    ..ChangeMetadata::default()
                },
                dependencies: below.into_iter().collect::<BTreeSet<_>>(),
                revision_metadata: metadata(),
                edits,
            },
        )
        .await?;
    tip.head = mutator.publish(prepared).await?;
    tip.effective = outcome.resulting_view;
    Ok(Latest {
        revision: outcome.revision_id,
        edit,
    })
}

/// Revises `change`'s latest revision with a new value for `path`.
async fn revise(
    scratch: &Scratch,
    mutator: &Mutator<'_>,
    tip: &mut Tip,
    change: ChangeId,
    path: &str,
    previous: Latest,
    checkpoint: usize,
) -> Result<Latest> {
    let value = format!("checkpoint {checkpoint}\n");
    let (edit, edits) = rewrite(
        &scratch.content,
        path,
        value.as_bytes(),
        Some(previous.edit),
    )?;
    let (prepared, outcome) = mutator
        .revise(
            tip.head,
            tip.base,
            tip.effective,
            change,
            previous.revision,
            metadata(),
            edits,
        )
        .await?;
    tip.head = mutator.publish(prepared).await?;
    tip.effective = outcome.resulting_view;
    Ok(Latest {
        revision: outcome.revision_id,
        edit,
    })
}

fn metadata() -> RevisionMetadata {
    RevisionMetadata {
        author: author(),
        committer: author(),
        ..RevisionMetadata::default()
    }
}
