//! Git for the cases that build a Git fixture, with a fixed author and date.

use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};

/// Feeds `stream` to `git fast-import` in `root`, which must succeed. Much faster
/// than one `git commit` per commit for long histories.
pub(crate) fn fast_import(root: &Path, stream: &[u8]) -> Result<()> {
    let mut command = Command::new("git");
    let mut child = crate::isolate(&mut command)
        .current_dir(root)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git fast-import")?;
    child
        .stdin
        .take()
        .context("git fast-import has no stdin")?
        .write_all(stream)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git fast-import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Runs `git <arguments>` in `root`, which must succeed.
pub(crate) fn git(root: &Path, arguments: &[&str]) -> Result<()> {
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
