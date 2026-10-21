//! Pulling a few Git commits into a long imported history.
//!
//! `wheat pull <git-url>` walks back from the branch tip to the newest commit the
//! workspace holds, appends the commits above it, and moves local work above them. Its
//! work should grow with the commits pulled, not with the history below them. Two
//! sides each import a Git history of one-file commits, `early` and `history` long,
//! make a local change, and pull `pulled` new commits: first onto the imported
//! history, then, after pushing the local change, onto the pushed commit, which pull
//! recognises from the projection. The records each pull adds to the store, per
//! pulled commit, must not grow much from the short history to the long one.
//!
//! Recognising the pushed commit re-projects its whole tree, as push does for its
//! anchor, so that pull also reads every file once: `files` files. And, like every
//! local command until incremental-validation step 9, pull validates the whole
//! history first (`frontier_service::list`): 2.7 of the long side's 3.2 s on
//! 2026-10-21, so its wall time is recorded without a budget.

use std::{fmt::Write as _, fs};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use super::{
    CaseContext, CaseOutput, Recorder,
    git::{fast_import, git},
    workspace::Workspace,
};
use crate::path_text;

pub(crate) const NAME: &str = "git_pull_incremental";
/// Budget for the records one pulled commit adds on the long history, as a percentage
/// of the same on the short one. Measured on 2026-10-21 (release): 17.2 → 32.0 records
/// onto the import and 16.9 → 31.3 onto the push, 186%, as the radix maps get a level
/// deeper and the tree spreads over 32 directories instead of one. A pull that
/// rewrote state growing with the history would be near 4,000%.
const RECORD_GROWTH_BUDGET_PERCENT: f64 = 250.0;
/// Files per directory, like a source tree's.
const FILES_PER_DIRECTORY: usize = 64;
/// The commit time of every fixture commit.
const COMMIT_TIME: &str = "1767225600 +0000";

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let history = context.scale.pick(2_000, 20);
    let early = context.scale.pick(50, 5);
    let pulled = context.scale.pick(20, 3);
    let files = context.scale.pick(2_000, 20);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("history", history);
    recorder.parameter("early", early);
    recorder.parameter("pulled", pulled);
    recorder.parameter("files", files);

    let mut per_commit = Vec::new();
    for (side, commits) in [("early", early), ("late", history)] {
        let side_context = CaseContext {
            scale: context.scale,
            wheat: context.wheat.clone(),
            scratch: context.scratch.join(side),
        };
        fs::create_dir_all(&side_context.scratch)?;
        per_commit.push(pull_side(
            &side_context,
            side,
            commits,
            pulled,
            files,
            &mut recorder,
        )?);
    }
    for (index, step) in ["pull", "pull_after_push"].iter().enumerate() {
        let (early, late) = (per_commit[0][index], per_commit[1][index]);
        let name = format!("{step}.record_growth");
        recorder.value(&name, late / early * 100.0, "percent");
        recorder.at_most(&name, RECORD_GROWTH_BUDGET_PERCENT, "percent");
    }
    Ok(recorder.finish())
}

/// Imports a `commits`-long history and pulls into it twice; returns the records each
/// pull added per pulled commit.
fn pull_side(
    context: &CaseContext,
    side: &str,
    commits: usize,
    pulled: usize,
    files: usize,
    recorder: &mut Recorder,
) -> Result<[f64; 2]> {
    let source = context.scratch.join("remote.git");
    fs::create_dir_all(&source)?;
    git(&source, &["init", "-q", "--bare", "-b", "main"])?;
    fast_import(&source, &commit_stream(0, commits, files))?;
    let (workspace, imported) = Workspace::import(context, &source)?;
    recorder.timing(&format!("{side}.import"), imported.timing);
    let url = path_text(&source)?;

    workspace.ok(&["change", "new", "--message", "local work"])?;
    workspace.write("local.txt", "local\n")?;
    workspace.ok(&["add", "local.txt"])?;

    // Onto the imported history.
    fast_import(&source, &commit_stream(commits, pulled, files))?;
    let onto_import = pull(&workspace, url, pulled, 1)?;
    recorder.timing(&format!("{side}.pull"), onto_import.timing);

    // Onto the pushed local change.
    workspace.ok(&["push", url, "--to", "refs/heads/main"])?;
    fast_import(&source, &commit_stream(commits + pulled, pulled, files))?;
    let onto_push = pull(&workspace, url, pulled, 0)?;
    recorder.timing(&format!("{side}.pull_after_push"), onto_push.timing);
    workspace.ok(&["validate"])?;
    ensure!(
        workspace.read("local.txt")? == "local\n",
        "the local change was lost"
    );

    let per_commit =
        [onto_import.records, onto_push.records].map(|records| records / pulled as f64);
    recorder.value(
        &format!("{side}.pull.records_per_commit"),
        per_commit[0],
        "count",
    );
    recorder.value(
        &format!("{side}.pull_after_push.records_per_commit"),
        per_commit[1],
        "count",
    );
    Ok(per_commit)
}

struct Pulled {
    timing: crate::measure::Timing,
    records: f64,
}

/// Pulls `url` and checks it appended `pulled` commits and moved `rebased` changes.
fn pull(workspace: &Workspace, url: &str, pulled: usize, rebased: usize) -> Result<Pulled> {
    let before = records(workspace)?;
    let captured = workspace.ok(&["--format=json", "pull", url])?;
    let after = records(workspace)?;
    let document: Value = serde_json::from_slice(&captured.stdout)?;
    let result = &document["result"];
    let count = |field: &str| result[field].as_array().map_or(0, Vec::len);
    ensure!(
        count("appended") == pulled && count("rebased") == rebased,
        "unexpected pull result {result}"
    );
    Ok(Pulled {
        timing: captured.timing,
        records: after - before,
    })
}

/// The records in the workspace's store.
fn records(workspace: &Workspace) -> Result<f64> {
    let captured = workspace.ok(&["--format=json", "store", "analyze"])?;
    let document: Value = serde_json::from_slice(&captured.stdout)?;
    let result = &document["result"];
    [
        "segment_entries",
        "loose_object_count",
        "loose_content_count",
    ]
    .iter()
    .map(|field| {
        result[field]
            .as_f64()
            .with_context(|| format!("store analyze has no {field}"))
    })
    .sum()
}

/// A fast-import stream of `count` commits on `main` after the first `first`, each
/// writing one of `files` files.
fn commit_stream(first: usize, count: usize, files: usize) -> Vec<u8> {
    let mut stream = String::new();
    for number in first..first + count {
        let message = format!("commit {number}");
        let file = number % files;
        let path = format!("src/d{:03}/f{file:05}.txt", file / FILES_PER_DIRECTORY);
        let contents = format!("file {file}\nversion {number}\n");
        // Later commits of one stream follow the one before on their own.
        let from = if number == first && first > 0 {
            "from refs/heads/main^0\n"
        } else {
            ""
        };
        write!(
            stream,
            "commit refs/heads/main\ncommitter Wheat Bench <wheat-bench@example.invalid> \
             {COMMIT_TIME}\ndata {}\n{message}\n{from}M 100644 inline {path}\ndata {}\n\
             {contents}\n",
            message.len(),
            contents.len()
        )
        .expect("writing to a string succeeds");
    }
    stream.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_continued_stream_starts_from_the_branch() {
        let first = String::from_utf8(commit_stream(0, 2, 10)).unwrap();
        assert!(!first.contains("from "));
        let next = String::from_utf8(commit_stream(2, 2, 10)).unwrap();
        assert_eq!(next.matches("from refs/heads/main^0").count(), 1);
    }

    #[test]
    fn commits_spread_over_directories() {
        let stream = String::from_utf8(commit_stream(64, 1, 128)).unwrap();
        assert!(stream.contains("M 100644 inline src/d001/f00064.txt"));
    }
}
