//! One large file through the Git bridge: pushed, edited by one byte and pushed again,
//! and imported fresh.
//!
//! A workspace adds a `file_mib` MiB file and pushes it to a bare Git repository, then
//! flips one byte in the middle and pushes that. Git keeps each version whole, so what
//! the pushes write to it is recorded without a budget. A fresh import of the branch
//! stores the first version and only the chunks the second changed.
//!
//! The peaks are recorded without a budget: they are git's. At 1 GiB (2026-11-05) push
//! and import peak at about 1.1 GiB, of which `wheat` itself is 36–72 MiB; the rest is
//! git mapping the object file (`pack-objects` held 0.7 MiB of heap and 1080 MiB of
//! mapped file). Plain `git push` and `git cat-file` of the same blob peak at 1028 MiB
//! without Wheat, so it is what git does, not how Wheat calls it.

use std::fs;

use anyhow::{Result, ensure};
use wheat_core::MAX_CONTENT_CHUNK_BYTES;

use super::{
    CaseContext, CaseOutput, Recorder,
    content_large_blob::directory_bytes,
    git::git,
    large_file::{allow_file, file_id, flip_byte},
    publish_content::write_noise,
    workspace::Workspace,
};
use crate::path_text;

pub(crate) const NAME: &str = "large_file_git";
const MIB: usize = 1024 * 1024;
const PATH: &str = "large.bin";
/// What the import may store beyond the first version: the chunks the edit touched, a
/// manifest, the history's records and the import journal.
const EDIT_BUDGET_BYTES: f64 = 2.0 * MAX_CONTENT_CHUNK_BYTES as f64 + 2.0 * MIB as f64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let file_mib = context.scale.pick(1024, 24);
    let length = file_mib * MIB;
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("file_mib", file_mib);

    let workspace = Workspace::new(context)?;
    let remote = context.scratch.join("remote.git");
    fs::create_dir_all(&remote)?;
    git(&remote, &["init", "-q", "--bare", "-b", "main"])?;
    let url = path_text(&remote)?;

    let path = workspace.path(PATH);
    allow_file(workspace.root(), length)?;
    write_noise(&path, 0, length)?;
    workspace.ok(&["change", "new", "--message", "large file"])?;
    workspace.ok(&["add", PATH])?;
    let pushed = workspace.ok(&["push", url, "--to", "refs/heads/main"])?;
    recorder.timing("push", pushed.timing);
    recorder.value("push.git_bytes", directory_bytes(&remote)? as f64, "bytes");

    flip_byte(&path, length as u64 / 2)?;
    workspace.ok(&["change", "new", "--message", "one byte"])?;
    workspace.ok(&["add", PATH])?;
    let git_before = directory_bytes(&remote)?;
    let pushed = workspace.ok(&["push", url, "--to", "refs/heads/main"])?;
    recorder.timing("edit_push", pushed.timing);
    recorder.value(
        "edit_push.git_bytes",
        (directory_bytes(&remote)? - git_before) as f64,
        "bytes",
    );

    let imported = context.scratch.join("imported");
    let stores = context.scratch.join("imported-stores");
    let import = workspace.ok_in_stores(
        &context.scratch,
        &stores,
        &[
            "import",
            url,
            path_text(&imported)?,
            "--branch",
            "refs/heads/main",
        ],
    )?;
    recorder.timing("import", import.timing);
    let stored = directory_bytes(&stores)? as f64;
    recorder.value("import.bytes_stored", stored, "bytes");
    ensure!(
        file_id(&imported.join(PATH))? == file_id(&path)?,
        "the imported file differs from the pushed edit"
    );

    // Both versions imported, but the second shares all but a few chunks.
    recorder.at_most(
        "import.bytes_stored",
        length as f64 + EDIT_BUDGET_BYTES,
        "bytes",
    );
    Ok(recorder.finish())
}
