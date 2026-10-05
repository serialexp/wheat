//! Writing many small files into a workspace, and replacing them all.
//!
//! Materialization once synced every file about four times, one after another, so
//! importing 4,096 small files took 30 s on NVMe. It now stages up to 256 files at a
//! time on several threads, records the batch with one progress record, and syncs
//! each directory once per batch. Here `import` writes `files` files, 64 per
//! directory, and two switches replace every one of them.

use std::{fs, path::Path, time::Instant};

use anyhow::{Result, ensure};

use super::{CaseContext, CaseOutput, Recorder, git::git, workspace::Workspace};

pub(crate) const NAME: &str = "materialize_wide";
/// Budget for `import`. Measured on 2026-10-05 (release, NVMe ext4, 20,000 files):
/// 145 s before, 9.6 s after.
const IMPORT_BUDGET_SECONDS: f64 = 20.0;
/// Budget for one switch. Measured as above: 175 s and 198 s before, 6.7 s and
/// 7.1 s after. Most of what is left is the staged files' own syncs.
const SWITCH_BUDGET_SECONDS: f64 = 15.0;
/// Files per directory, like a source tree's.
const FILES_PER_DIRECTORY: usize = 64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(20_000, 300);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("files", files);

    let source = context.scratch.join("git");
    let started = Instant::now();
    build_git_history(&source, files)?;
    recorder.wall("git_fixture", started.elapsed().as_secs_f64());

    let (workspace, imported) = Workspace::import(context, &source)?;
    recorder.timing("import", imported.timing);
    check_version(&workspace, files, "main")?;

    recorder.timing(
        "switch_feature",
        workspace.ok(&["switch", "feature"])?.timing,
    );
    check_version(&workspace, files, "feature")?;
    recorder.timing("switch_main", workspace.ok(&["switch", "main"])?.timing);
    check_version(&workspace, files, "main")?;

    recorder.at_most("import.wall", IMPORT_BUDGET_SECONDS, "s");
    recorder.at_most("switch_feature.wall", SWITCH_BUDGET_SECONDS, "s");
    recorder.at_most("switch_main.wall", SWITCH_BUDGET_SECONDS, "s");
    Ok(recorder.finish())
}

/// `main` and `feature` each hold every file, with different contents. They are
/// siblings, so each label has one frontier.
fn build_git_history(root: &Path, files: usize) -> Result<()> {
    fs::create_dir_all(root)?;
    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["commit", "-q", "--allow-empty", "-m", "root"])?;
    git(root, &["checkout", "-q", "-b", "feature"])?;
    write_files(root, files, "feature")?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", "feature files"])?;
    git(root, &["checkout", "-q", "main"])?;
    write_files(root, files, "main")?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", "main files"])?;
    Ok(())
}

fn write_files(root: &Path, files: usize, version: &str) -> Result<()> {
    for index in 0..files {
        let path = root.join(file_path(index));
        if index % FILES_PER_DIRECTORY == 0 {
            fs::create_dir_all(path.parent().expect("files live in a directory"))?;
        }
        fs::write(&path, file_contents(index, version))?;
    }
    Ok(())
}

/// The first and last file hold `version`'s contents.
fn check_version(workspace: &Workspace, files: usize, version: &str) -> Result<()> {
    for index in [0, files - 1] {
        let path = file_path(index);
        ensure!(
            workspace.read(&path)? == file_contents(index, version),
            "{path} does not hold {version}'s contents"
        );
    }
    Ok(())
}

fn file_path(index: usize) -> String {
    format!("d{:04}/f{index:06}.txt", index / FILES_PER_DIRECTORY)
}

fn file_contents(index: usize, version: &str) -> String {
    format!("file {index} on {version}\n")
}
