//! Git for the cases that build a Git fixture, with a fixed author and date.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, ensure};

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
