//! Placing an edit under a deep stack (formerly the ignored
//! `edit_move::placing_an_edit_under_a_deep_stack_stays_fast` test).
//!
//! Change B holds a big file (about 1 MiB at full size) and a small one. Above it sit
//! `changes` more changes; every tenth rewrites its own line of the big file, the rest
//! each add a file of their own. Any add to B reapplies every change above it:
//!
//! - `reapply`: revise B's small file, which no later change touches;
//! - `fitting`: edit one of B's own lines of the big file far from any later block;
//! - `moving`: overlap a block a later change wrote, which moves that block into B.
//!
//! Each step runs three times on different lines and records the median. Placing the
//! edit must cost at most 1.5 s on top of the reapplication it always pays for.

use std::time::Instant;

use anyhow::{Result, ensure};

use super::{
    CaseContext, CaseOutput, Recorder,
    workspace::{Workspace, field, stdout},
};
use crate::measure::Timing;

pub(crate) const NAME: &str = "deep_stack";
/// Lines between two later changes' blocks in the big file.
const BLOCK_SPACING: usize = 200;
/// Allowed cost of placing an edit on top of reapplying the stack.
const PLACEMENT_BUDGET_SECONDS: f64 = 1.5;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(500, 60);
    // Change S{10k} writes block k at line k * BLOCK_SPACING + 100.
    let blocks = changes / 10;
    let lines = blocks * BLOCK_SPACING + 1_000;
    let block_line = |block: usize| block * BLOCK_SPACING + 100;
    // Three blocks a quarter of the way up the stack (S120, S140, S160 at full size).
    let moving_lines = [0, 2, 4].map(|offset| block_line(blocks / 4 + offset));
    // Three of B's own lines halfway up, 50 lines from the nearest block.
    let fitting_lines = [0, 1, 2].map(|offset| (blocks / 2 + offset) * BLOCK_SPACING + 150);
    ensure!(
        blocks / 4 + 4 < blocks,
        "deep_stack needs at least 60 changes"
    );

    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("big_file_lines", lines);

    let line = |n: usize, tag: &str| format!("{n:05} {tag} {}\n", "x".repeat(90));
    let setup_started = Instant::now();
    let workspace = Workspace::new(context)?;
    let b = field(&workspace.ok(&["change", "new", "-m", "B"])?, "change")?;
    let mut big = (0..lines).map(|n| line(n, "base")).collect::<Vec<_>>();
    workspace.write("big", big.concat())?;
    workspace.write("own", "one\n")?;
    workspace.ok(&["add", "-m", "B1", "big", "own"])?;
    recorder.parameter("big_file_bytes", big.concat().len());

    for n in 0..changes {
        let message = format!("S{n}");
        if n % 10 == 0 {
            workspace.ok(&["change", "new", "-m", &message])?;
            let at = block_line(n / 10);
            big[at] = line(at, &message);
            workspace.write("big", big.concat())?;
            workspace.ok(&["add", "-m", &message, "big"])?;
        } else {
            let path = format!("other-{n}");
            workspace.ok(&[
                "change", "new", "-m", &message, "--path", &path, "--value", "x\n",
            ])?;
        }
    }
    workspace.ok(&["materialize"])?;
    recorder.wall("setup", setup_started.elapsed().as_secs_f64());

    let mut reapply = Vec::with_capacity(3);
    for value in ["two", "three", "four"] {
        workspace.write("own", format!("{value}\n"))?;
        reapply.push(
            workspace
                .ok(&["add", "-m", "B own", "--change", &b, "own"])?
                .timing,
        );
    }

    let mut fitting = Vec::with_capacity(3);
    for at in fitting_lines {
        big[at] = line(at, "mine");
        workspace.write("big", big.concat())?;
        let fits = workspace.ok(&["add", "-m", "B fits", "--change", &b, "big"])?;
        ensure!(
            !stdout(&fits).contains("moved"),
            "an edit on B's own line moved a block: {}",
            stdout(&fits)
        );
        fitting.push(fits.timing);
    }

    let mut moving = Vec::with_capacity(3);
    for at in moving_lines {
        big[at] = line(at, "mine");
        workspace.write("big", big.concat())?;
        let moves = workspace.ok(&["add", "-m", "B moves", "--change", &b, "big", "--force"])?;
        let out = stdout(&moves);
        ensure!(
            out.matches("moved big: your edit").count() == 1,
            "an edit over a later block did not move exactly that block: {out}"
        );
        moving.push(moves.timing);
    }

    settled(&workspace, &big.concat())?;

    let reapply = recorder.runs("reapply", &reapply);
    record_placement(&mut recorder, "fitting", &fitting, reapply);
    record_placement(&mut recorder, "moving", &moving, reapply);
    Ok(recorder.finish())
}

/// `<step>` runs plus `<step>_overhead`, their median minus the reapplication median,
/// which must stay within the placement budget.
fn record_placement(recorder: &mut Recorder, step: &str, runs: &[Timing], reapply: f64) {
    let wall = recorder.runs(step, runs);
    let overhead = format!("{step}_overhead");
    recorder.value(&overhead, wall - reapply, "s");
    recorder.at_most(&overhead, PLACEMENT_BUDGET_SECONDS, "s");
}

/// The workspace matches the working view, the repository validates, and a fresh
/// materialization reproduces the big file.
fn settled(workspace: &Workspace, big: &str) -> Result<()> {
    let status = workspace.ok(&["status"])?;
    ensure!(
        stdout(&status).contains("tracked changes: 0"),
        "workspace should be clean: {}",
        stdout(&status)
    );
    workspace.ok(&["validate"])?;
    workspace.ok(&["materialize"])?;
    ensure!(
        workspace.read("big")? == big,
        "materialized big file differs"
    );
    Ok(())
}
