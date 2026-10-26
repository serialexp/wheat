//! The commands that move a label, on a repository with a long history and a change
//! revised many times (`docs/design/wheat-label-moves.md`).
//!
//! Two clones of one server. The writer builds `history` changes, each rewriting one of
//! [`HISTORY_FILES`] files so the tree stays the same size, and publishes them, then a
//! change it publishes once and revises `revisions` times more; the reader
//! clones and makes a change of its own. Then, each measured with the objects it read
//! from its store: the writer's `publish`, which moves the label from the change's first
//! revision to its last; the reader's `pull`, which fetches that and integrates its own
//! change with it; the writer's `label move` to one more revision; and, once the writer
//! has published that, the reader's `refresh`, which integrates its joined view with the
//! label's new one. One change differs each time.
//!
//! A move compares the two views' revision maps and merges only the changes that
//! differ, so what these commands read grows with the history only as the collections
//! they read deepen, with its logarithm: from the shortest history to the longest, at
//! each revision count, each command's reads grow by at most [`READ_GROWTH_PER_BIT`]
//! percent of the shortest's per doubling of the history. Growing with the history
//! itself would be about 270% per doubling here. Revisions are not bounded: the merge
//! walks the moved change's revisions between the two views, and its reads are recorded
//! to see that slope.

use std::path::Path;

use anyhow::Result;

use super::{CaseContext, CaseOutput, Recorder, server::Server, workspace::Workspace};
use crate::path_text;

pub(crate) const NAME: &str = "label_move";
const HISTORY: [usize; 2] = [50, 500];
const REVISIONS: [usize; 2] = [10, 100];
/// The files the history changes rewrite in turn.
const HISTORY_FILES: usize = 10;
/// Most a command's reads may grow per doubling of the history, in percent of what it
/// read at the shortest. Measured 2026-10-26 (release, 10 revisions) at 50, 150, 500
/// and 1500 changes: publish 246, 255, 330, 399 reads; pull (fetch and integrate)
/// 576, 597, 741, 978; label move 24, 26, 30, 32; refresh 98, 103, 116, 142. That is
/// up to about 17% per doubling, against about 270% if it grew with the history, and
/// it comes in steps as the collections gain a level, which land at different sizes
/// from run to run; the fetch's incremental validation grows the most.
const READ_GROWTH_PER_BIT: f64 = 25.0;
const STEPS: [&str; 4] = ["publish", "pull", "label_move", "refresh"];

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let history = context.scale.pick(HISTORY.to_vec(), vec![3, 6]);
    let revisions = context.scale.pick(REVISIONS.to_vec(), vec![2, 3]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("history", &history);
    recorder.parameter("revisions", &revisions);
    let wheat = Workspace::new(context)?;

    for &revised in &revisions {
        let mut reads = Vec::with_capacity(history.len());
        for &changes in &history {
            let measured = measure(context, &wheat, changes, revised, &mut recorder)?;
            reads.push(measured);
        }
        let (first, last) = (&reads[0], &reads[reads.len() - 1]);
        let bits = (history[history.len() - 1] as f64 / history[0] as f64).log2();
        for (step, (small, large)) in STEPS.iter().zip(first.iter().zip(last)) {
            let name = format!("r{revised}.{step}.read_growth_per_bit");
            let growth = (*large as f64 / *small as f64 - 1.0) / bits * 100.0;
            recorder.value(&name, growth, "percent");
            recorder.at_most(&name, READ_GROWTH_PER_BIT, "percent");
        }
    }
    Ok(recorder.finish())
}

/// Runs the four commands on a repository of `changes` changes whose moved change has
/// `revised` revisions, and returns what each read, in [`STEPS`] order.
fn measure(
    context: &CaseContext,
    wheat: &Workspace,
    changes: usize,
    revised: usize,
    recorder: &mut Recorder,
) -> Result<[u64; 4]> {
    let root = context.scratch.join(format!("h{changes}-r{revised}"));
    let source = root.join("source");
    std::fs::create_dir_all(&root)?;
    wheat.ok_in(&root, &["init", path_text(&source)?])?;
    let server = Server::start(&source)?;
    let side = |name: &str| Side {
        workspace: root.join(name),
        stores: root.join(format!("{name}-stores")),
    };
    let (writer, reader) = (side("writer"), side("reader"));
    for side in [&writer, &reader] {
        std::fs::create_dir_all(&side.stores)?;
    }

    wheat.ok_in_stores(
        &root,
        &writer.stores,
        &["clone", server.url(), path_text(&writer.workspace)?],
    )?;
    for index in 0..changes {
        writer.history(wheat, index)?;
    }
    writer.ok(wheat, &["publish"])?;
    writer.change(wheat, "moved.txt")?;
    writer.ok(wheat, &["publish"])?;
    for revision in 1..revised {
        writer.revise(wheat, revision)?;
    }

    wheat.ok_in_stores(
        &root,
        &reader.stores,
        &["clone", server.url(), path_text(&reader.workspace)?],
    )?;
    reader.change(wheat, "local.txt")?;

    let step = |name: &str| format!("h{changes}.r{revised}.{name}");
    let mut reads = [0; 4];
    let mut record = |index: usize, (captured, read): (crate::measure::Captured, u64)| {
        recorder.timing(&step(STEPS[index]), captured.timing);
        recorder.value(
            &format!("{}.reads", step(STEPS[index])),
            read as f64,
            "count",
        );
        reads[index] = read;
    };
    record(0, writer.counted(wheat, &["publish"])?);
    record(1, reader.counted(wheat, &["pull"])?);
    writer.revise(wheat, revised)?;
    record(2, writer.counted(wheat, &["label", "move", "main"])?);
    writer.ok(wheat, &["publish"])?;
    reader.ok(wheat, &["fetch"])?;
    record(3, reader.counted(wheat, &["refresh"])?);

    anyhow::ensure!(
        std::fs::read_to_string(reader.workspace.join("moved.txt"))? == revised.to_string(),
        "the reader does not have the moved change's last revision"
    );
    server.stop()?;
    Ok(reads)
}

/// One clone: its workspace and its own store home.
struct Side {
    workspace: std::path::PathBuf,
    stores: std::path::PathBuf,
}

impl Side {
    fn ok(&self, wheat: &Workspace, arguments: &[&str]) -> Result<()> {
        wheat.ok_in_stores(&self.workspace, &self.stores, arguments)?;
        Ok(())
    }

    fn counted(
        &self,
        wheat: &Workspace,
        arguments: &[&str],
    ) -> Result<(crate::measure::Captured, u64)> {
        wheat.counted_in_stores(&self.workspace, &self.stores, arguments)
    }

    /// A new change writing `path`.
    fn change(&self, wheat: &Workspace, path: &str) -> Result<()> {
        self.ok(
            wheat,
            &[
                "change",
                "new",
                "--path",
                path,
                "--value",
                path,
                "--message",
                path,
            ],
        )
    }

    /// History change `index`: one of the [`HISTORY_FILES`] files becomes `index`.
    fn history(&self, wheat: &Workspace, index: usize) -> Result<()> {
        let path = format!("history/{}.txt", index % HISTORY_FILES);
        let value = index.to_string();
        let mut arguments = vec!["change", "new", "--path", &path, "--value", &value];
        let expected = index.checked_sub(HISTORY_FILES).map(|was| was.to_string());
        if let Some(expected) = &expected {
            arguments.extend(["--expected", expected]);
        }
        arguments.extend(["--message", &value]);
        self.ok(wheat, &arguments)
    }

    /// Revises the moved change: `moved.txt` becomes `revision`.
    fn revise(&self, wheat: &Workspace, revision: usize) -> Result<()> {
        write(&self.workspace, "moved.txt", &revision.to_string())?;
        self.ok(wheat, &["add", "moved.txt"])
    }
}

fn write(workspace: &Path, path: &str, contents: &str) -> Result<()> {
    std::fs::write(workspace.join(path), contents)?;
    Ok(())
}
