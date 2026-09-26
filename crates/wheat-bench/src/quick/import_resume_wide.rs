//! Resuming a staged Git import after a commit that touches many files.
//!
//! Each `stage_import` call first re-checks the mapping of the commit it builds on:
//! it replays that commit's edits, matches every outcome back to its edit, and looks
//! every touched path up in the parent and resulting trees. The match was a linear
//! search per outcome (`files²` comparisons), and each lookup decoded the path's
//! directories again, so a wide directory was decoded once per file in it. Here one
//! commit adds `files` files, 1,000 per directory, and the next changes one; the call
//! that stages the second commit re-checks the first, and must stay within its budget.

use std::{fs, path::Path, process::Command, time::Instant};

use anyhow::{Context, Result, ensure};
use wheat_git::stage_import;

use super::{CaseContext, CaseOutput, Recorder, history::Scratch};

pub(crate) const NAME: &str = "import_resume_wide";
/// Budget for the resume that re-checks the wide commit. Measured on 2026-09-26
/// (release, 100,000 files): 20.8 s before, 15.9 s with only the search fixed, 2.4 s
/// with the lookups sharing one walk per tree.
const RESUME_BUDGET_SECONDS: f64 = 6.0;
/// Files per directory, so no Git tree gets unrealistically wide.
const FILES_PER_DIRECTORY: usize = 1_000;
/// Stop instead of looping forever if staging makes no progress.
const MAX_STAGE_CALLS: usize = 16;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(100_000, 300);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("files", files);

    let git = context.scratch.join("git");
    let started = Instant::now();
    build_git_history(&git, files)?;
    recorder.wall("git_fixture", started.elapsed().as_secs_f64());

    let scratch = Scratch::open(&context.scratch.join("wheat"))?;
    let journal = context.scratch.join("import.sqlite");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let started = Instant::now();
        let mut calls = 0;
        loop {
            let progress =
                stage_import(&git, &journal, &scratch.objects, &scratch.content, Some(1)).await?;
            if progress.completed_commits >= 1 {
                break;
            }
            calls += 1;
            ensure!(calls < MAX_STAGE_CALLS, "the wide commit never staged");
        }
        recorder.wall("stage_wide", started.elapsed().as_secs_f64());

        let started = Instant::now();
        let progress =
            stage_import(&git, &journal, &scratch.objects, &scratch.content, Some(1)).await?;
        recorder.wall("resume", started.elapsed().as_secs_f64());
        ensure!(
            progress.completed_commits == 2 && progress.complete,
            "the resume did not finish the import: {} commits done, {} pending",
            progress.completed_commits,
            progress.pending_commits
        );
        Ok(())
    })?;
    recorder.at_most("resume.wall", RESUME_BUDGET_SECONDS, "s");
    Ok(recorder.finish())
}

/// One commit adding `files` small files, then one changing a single file.
fn build_git_history(root: &Path, files: usize) -> Result<()> {
    fs::create_dir_all(root)?;
    git(root, &["init", "-q", "-b", "main"])?;
    for index in 0..files {
        let path = root.join(file_path(index));
        if index % FILES_PER_DIRECTORY == 0 {
            fs::create_dir_all(path.parent().expect("files live in a directory"))?;
        }
        fs::write(&path, format!("file {index}\n"))?;
    }
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", "add many files"])?;
    fs::write(root.join(file_path(0)), "file 0, changed\n")?;
    git(root, &["commit", "-q", "-a", "-m", "change one file"])?;
    Ok(())
}

fn file_path(index: usize) -> String {
    format!("d{:03}/f{index:06}.txt", index / FILES_PER_DIRECTORY)
}

fn git(root: &Path, arguments: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    let output = crate::isolate(&mut command)
        .current_dir(root)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Wheat Bench")
        .env("GIT_AUTHOR_EMAIL", "wheat-bench@example.invalid")
        .env("GIT_COMMITTER_NAME", "Wheat Bench")
        .env("GIT_COMMITTER_EMAIL", "wheat-bench@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .with_context(|| format!("run git {arguments:?}"))?;
    ensure!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
