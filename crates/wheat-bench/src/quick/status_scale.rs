//! `status` on a workspace of many small files and one long binary file, through the
//! workspace's stat cache.
//!
//! The workspace's one change adds [`DIRECTORIES`] directories of small files and a
//! long file of noise (1.25 GiB at full scale), every file written an hour ago so the
//! cache may keep it. The
//! first status reads every file and records it. With nothing changed, a status reads
//! no file and no baseline. With ten small files edited, it reads those ten. With a
//! byte of the long file changed at one of its samples, human status (which does not
//! need content IDs) reads the samples and not the file; JSON status reads it whole.
//! Counts come from the scan itself, with status run in this process. They are checked
//! at every scale; at full scale the contract also holds the warm status to a time
//! budget and the case's peak to one far below the long file.

use std::{
    fs,
    io::Write as _,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, ensure};
use wheat_client::{
    workspace_scan::ContentIds,
    workspace_status::{self, WorkspaceStatus},
};

use super::{
    CaseContext, CaseOutput, Recorder, large_file::allow_file, publish_content::write_noise,
    workspace::Workspace,
};

pub(crate) const NAME: &str = "status_scale";
const DIRECTORIES: usize = 100;
const MIB: usize = 1024 * 1024;
/// Budget for a status with nothing changed: no file read, so a few stats and a
/// cache lookup per file.
const WARM_BUDGET_SECONDS: f64 = 1.0;
/// Budget for the case's peak: status streams each file, so neither the long file
/// nor the bytes read in all show in it. 102 MiB at 1.25 GiB (2026-11-06).
const PEAK_BUDGET_BYTES: f64 = 192.0 * MIB as f64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(20_000, 300);
    // Over 1 GiB at full scale: status once refused a file that long.
    let long_mib = context.scale.pick(1280, 20);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("files", files);
    recorder.parameter("long_mib", long_mib);

    let workspace = Workspace::new(context)?;
    let root = workspace.root().to_owned();
    let paths = (0..files)
        .map(|index| format!("d{:03}/f{index:06}.txt", index % DIRECTORIES))
        .collect::<Vec<_>>();
    for (index, path) in paths.iter().enumerate() {
        workspace.write(path, format!("file {index}\n").repeat(400))?;
    }
    allow_file(&root, long_mib * MIB)?;
    write_noise(&root.join("long.bin"), 0, long_mib * MIB)?;
    workspace.ok(&["change", "new", "--message", "files"])?;
    let mut add = vec!["add", "--message", "files", ".wheat.json", "long.bin"];
    add.extend(paths.iter().map(String::as_str));
    workspace.ok(&add)?;
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for path in paths
        .iter()
        .map(String::as_str)
        .chain([".wheat.json", "long.bin"])
    {
        set_mtime(&root.join(path), an_hour_ago)?;
    }

    let total = files + 2;
    let cold = step(&mut recorder, &root, "cold", ContentIds::Required)?;
    ensure!(cold.changes.is_empty(), "the workspace is not clean");
    ensure!(cold.reads.files_read == total, "cold read {:?}", cold.reads);
    let warm = step(&mut recorder, &root, "warm", ContentIds::Required)?;
    ensure!(warm.changes.is_empty(), "the workspace is not clean");
    ensure!(
        warm.reads.files_read == 0 && warm.reads.baselines_read == 0,
        "warm read {:?}",
        warm.reads
    );

    for path in paths.iter().step_by(files / 10).take(10) {
        workspace.write(path, "edited\n")?;
        set_mtime(&root.join(path), an_hour_ago)?;
    }
    let edited = step(&mut recorder, &root, "edited", ContentIds::Required)?;
    ensure!(
        edited.changes.len() == 10,
        "{} changes",
        edited.changes.len()
    );
    ensure!(
        edited.reads.files_read == 10,
        "edited read {:?}",
        edited.reads
    );

    let mut long = fs::File::options()
        .write(true)
        .open(root.join("long.bin"))?;
    long.write_all(b"\xff\x00")?;
    drop(long);
    set_mtime(&root.join("long.bin"), an_hour_ago)?;
    let sampled = step(&mut recorder, &root, "long_sampled", ContentIds::Optional)?;
    ensure!(
        sampled.changes.len() == 11,
        "{} changes",
        sampled.changes.len()
    );
    ensure!(
        sampled.reads.files_read == 0 && sampled.reads.files_sampled == 1,
        "sampled read {:?}",
        sampled.reads
    );
    let identified = step(
        &mut recorder,
        &root,
        "long_identified",
        ContentIds::Required,
    )?;
    ensure!(
        identified
            .changes
            .iter()
            .all(|change| change.workspace.is_some()),
        "a change lacks its content ID"
    );

    recorder.at_most("warm.files_read", 0.0, "count");
    recorder.at_most("warm.baselines_read", 0.0, "count");
    recorder.at_most("warm.wall", WARM_BUDGET_SECONDS, "s");
    recorder.at_most("total.peak_rss", PEAK_BUDGET_BYTES, "bytes");
    recorder.at_most("edited.files_read", 10.0, "count");
    recorder.at_most("long_sampled.files_read", 0.0, "count");
    recorder.at_most("long_sampled.files_sampled", 1.0, "count");
    Ok(recorder.finish())
}

/// Runs status in this process and records what it read and how long it took.
fn step(
    recorder: &mut Recorder,
    root: &Path,
    name: &str,
    ids: ContentIds,
) -> Result<WorkspaceStatus> {
    let started = Instant::now();
    let status = workspace_status::read(root, None, ids)?;
    recorder.wall(name, started.elapsed().as_secs_f64());
    let reads = status.reads;
    recorder.value(
        &format!("{name}.files_read"),
        reads.files_read as f64,
        "count",
    );
    recorder.value(
        &format!("{name}.bytes_read"),
        reads.bytes_read as f64,
        "bytes",
    );
    recorder.value(
        &format!("{name}.files_sampled"),
        reads.files_sampled as f64,
        "count",
    );
    recorder.value(
        &format!("{name}.baselines_read"),
        reads.baselines_read as f64,
        "count",
    );
    Ok(status)
}

fn set_mtime(path: &Path, time: SystemTime) -> Result<()> {
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(time)?;
    Ok(())
}
