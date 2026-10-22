//! `wheat ignore import`: give every `.gitignore` the ignore fallback reads a
//! sibling `.wheatignore`, so the rules are explicit.
//!
//! The files are found by the same walk `wheat status` makes, so this imports
//! exactly the `.gitignore` files whose fallback diagnostic `status` prints: those
//! in directories the walk enters and that have no `.wheatignore`. Each copy drops
//! the lines that name Git's own files (`.git`, `.gitmodules`, `.gitattributes`).
//! A copy is written under a temporary name and renamed into place only if no
//! `.wheatignore` has appeared meanwhile, so an existing file is never replaced.
//! The new files are not captured; the user adds them like any other file.

use std::{io::Write as _, path::Path};

use anyhow::{Context, Result};
use serde::Serialize;
use wheat_core::FileKind;
use wheat_store::{MerkleTreeStore, TreeWalkLimits};

use crate::{
    local::LocalRepository,
    sys::{self, Creation, OpenFile, Rename},
    workspace_files::read_workspace_file,
    workspace_metadata::WorkspaceMetadata,
    workspace_walk::{WalkLimits, walk_workspace},
};

/// The largest `.gitignore` this imports.
const MAX_IGNORE_FILE_BYTES: usize = 1024 * 1024;

/// Pattern names, without anchors or slashes, that only mean something to Git.
const GIT_ONLY_NAMES: [&str; 3] = [".git", ".gitmodules", ".gitattributes"];

#[derive(Debug, Default, Serialize)]
pub struct ImportedIgnores {
    /// The `.wheatignore` files written, as workspace-relative paths.
    pub written: Vec<String>,
    /// `.gitignore` files left alone, and why.
    pub skipped: Vec<SkippedIgnore>,
    /// The walk hit its limits, so some directories were not looked at.
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct SkippedIgnore {
    pub path: String,
    pub reason: &'static str,
}

/// Imports every `.gitignore` the fallback reads in the workspace at `root`.
pub fn import(
    root: &Path,
    repository: &LocalRepository,
    workspace: &WorkspaceMetadata,
) -> Result<ImportedIgnores> {
    let _lock = crate::materialization::WorkspaceLock::acquire(root)?;
    let runtime = crate::local::runtime()?;
    let mut workspace = workspace.clone();
    runtime.block_on(crate::materialization::recover_locked(
        root,
        repository,
        &mut workspace,
    ))?;
    let materialized = workspace
        .materialized
        .as_ref()
        .context("Wheat workspace has not been materialized")?;
    let tracked = runtime
        .block_on(
            MerkleTreeStore::new(&repository.objects)
                .walk_files(materialized.tree, TreeWalkLimits::default()),
        )?
        .into_iter()
        .map(|file| file.path)
        .collect();

    let global = wheat_ignore::loader::read_global_ignore();
    let global = global
        .as_ref()
        .map(|(content, path)| (content.as_str(), path.clone()));
    let mut ignores = wheat_ignore::stack::IgnoreStack::new(global, &[]);
    let walk = walk_workspace(root, &tracked, &mut ignores, WalkLimits::default())?;

    let mut imported = ImportedIgnores {
        truncated: walk.truncated,
        ..ImportedIgnores::default()
    };
    for gitignore in ignores.gitignore_fallbacks() {
        let relative = relative_path(root, gitignore)?;
        match import_one(root, &relative)? {
            Ok(written) => imported.written.push(written),
            Err(reason) => imported.skipped.push(SkippedIgnore {
                path: relative,
                reason,
            }),
        }
    }
    Ok(imported)
}

/// `path` relative to `root`, with `/` separators.
fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside the workspace", path.display()))?;
    let parts = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .context("ignore file path is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

/// Writes the `.wheatignore` beside the `.gitignore` at `gitignore` and returns its
/// path, or says why it did not.
fn import_one(root: &Path, gitignore: &str) -> Result<Result<String, &'static str>> {
    let Some(snapshot) = read_workspace_file(root, Path::new(gitignore), MAX_IGNORE_FILE_BYTES)
        .with_context(|| format!("cannot read {gitignore}"))?
    else {
        return Ok(Err("it disappeared"));
    };
    if snapshot.kind == FileKind::Symlink {
        return Ok(Err("it is a symlink"));
    }
    let Ok(text) = String::from_utf8(snapshot.bytes) else {
        return Ok(Err("it is not UTF-8"));
    };

    let directory_path = gitignore
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let mut directory = sys::open_directory(root)?;
    for component in directory_path.split('/').filter(|part| !part.is_empty()) {
        directory = sys::open_directory_at(&directory, component)
            .with_context(|| format!("cannot open directory {directory_path}"))?;
    }
    let wheatignore = if directory_path.is_empty() {
        ".wheatignore".to_owned()
    } else {
        format!("{directory_path}/.wheatignore")
    };
    if sys::entry_kind_at(&directory, ".wheatignore")?.is_some() {
        return Ok(Err("a .wheatignore appeared beside it"));
    }

    let temporary = format!(".wheatignore.import-{}", uuid::Uuid::now_v7());
    let mut file = sys::open_file_at(
        &directory,
        &temporary,
        OpenFile {
            read: false,
            write: true,
            append: false,
            creation: Creation::New,
            mode: 0o644,
        },
    )
    .with_context(|| format!("cannot create a temporary file beside {wheatignore}"))?;
    let written = file
        .write_all(without_git_only_lines(&text).as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| {
        sys::rename_at(
            &directory,
            &temporary,
            &directory,
            ".wheatignore",
            Rename::NoReplace,
        )
    });
    if let Err(error) = renamed {
        let _ = sys::remove_file_at(&directory, &temporary);
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Ok(Err("a .wheatignore appeared beside it"));
        }
        return Err(error).with_context(|| format!("cannot write {wheatignore}"));
    }
    directory.sync_all()?;
    Ok(Ok(wheatignore))
}

/// `text` without the patterns that name Git's own files, whatever their anchors,
/// slashes or negation.
fn without_git_only_lines(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| {
            let pattern = line.trim();
            let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
            let pattern = pattern.strip_prefix("**/").unwrap_or(pattern);
            let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
            let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
            !GIT_ONLY_NAMES.contains(&pattern)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::without_git_only_lines;

    #[test]
    fn git_only_patterns_are_dropped_and_the_rest_kept_exactly() {
        let gitignore = "# build\ntarget/\n.git/\n/.gitmodules\n**/.gitattributes\n!.git\n\
                         *.log\n  .gitattributes  \n.github/\nkeep.git\n\nlast";
        assert_eq!(
            without_git_only_lines(gitignore),
            "# build\ntarget/\n*.log\n.github/\nkeep.git\n\nlast"
        );
    }
}
