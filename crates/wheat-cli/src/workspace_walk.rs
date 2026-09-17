//! Bounded workspace directory traversal for untracked file discovery.
//!
//! Walks the physical workspace directory tree, classifies each entry as
//! tracked (already in the Merkle tree), untracked, or ignored, and prunes
//! ignored directories to avoid descending into build outputs and dependency
//! caches.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use wheat_ignore::stack::{IgnoreMatch, IgnoreStack};

/// Configuration bounds for the workspace walk.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Wired in the next step (status integration).
pub(crate) struct WalkLimits {
    pub max_entries: usize,
    pub max_directories: usize,
    pub max_depth: usize,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_entries: 500_000,
            max_directories: 50_000,
            max_depth: 64,
        }
    }
}

/// Classification of a physical workspace entry not in the tracked tree.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum UntrackedClassification {
    Untracked,
    Ignored,
}

/// One untracked or ignored entry discovered during the workspace walk.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct UntrackedEntry {
    pub path: String,
    pub classification: UntrackedClassification,
    pub is_directory: bool,
}

/// Result of a workspace-wide directory walk.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct WorkspaceWalkResult {
    pub untracked: Vec<UntrackedEntry>,
    pub gitignore_fallback_used: bool,
    pub gitignore_fallback_paths: Vec<PathBuf>,
    pub truncated: bool,
}

/// Build a set of tracked directory prefixes from tracked paths.
///
/// For a tracked path like `"src/lib/foo.rs"`, this produces `""`, `"src"`,
/// and `"src/lib"` — directories that contain tracked content and should be
/// descended into even though they're not tracked files themselves.
fn tracked_directory_prefixes(tracked_paths: &BTreeSet<String>) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    dirs.insert(String::new()); // The workspace root is always "tracked".
    for path in tracked_paths {
        let mut prefix = String::new();
        for (i, component) in path.split('/').enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(component);
            // Don't add the final component (the file itself).
            if prefix.len() < path.len() {
                dirs.insert(prefix.clone());
            }
        }
    }
    dirs
}

/// Walk the workspace directory tree and classify untracked entries.
#[allow(dead_code)]
pub(crate) fn walk_workspace(
    workspace_root: &Path,
    tracked_paths: &BTreeSet<String>,
    ignore_stack: &mut IgnoreStack,
    limits: WalkLimits,
) -> anyhow::Result<WorkspaceWalkResult> {
    let tracked_dirs = tracked_directory_prefixes(tracked_paths);
    let mut state = WalkState {
        tracked_paths,
        tracked_dirs: &tracked_dirs,
        limits: &limits,
        entries_visited: 0,
        directories_visited: 0,
        untracked: Vec::new(),
        truncated: false,
    };

    ignore_stack.push_directory(workspace_root);
    walk_directory(
        workspace_root,
        &String::new(),
        0,
        ignore_stack,
        &mut state,
    );
    ignore_stack.pop_directory();

    Ok(WorkspaceWalkResult {
        untracked: {
            state.untracked.sort_by(|a, b| a.path.cmp(&b.path));
            state.untracked
        },
        gitignore_fallback_used: !ignore_stack.gitignore_fallbacks().is_empty(),
        gitignore_fallback_paths: ignore_stack.gitignore_fallbacks().to_vec(),
        truncated: state.truncated,
    })
}

/// Mutable walk state shared across recursive calls.
struct WalkState<'a> {
    tracked_paths: &'a BTreeSet<String>,
    tracked_dirs: &'a BTreeSet<String>,
    limits: &'a WalkLimits,
    entries_visited: usize,
    directories_visited: usize,
    untracked: Vec<UntrackedEntry>,
    truncated: bool,
}

/// Recursively walk one directory, keeping parent ignore rules alive on
/// the call stack.
fn walk_directory(
    dir_path: &Path,
    prefix: &String,
    depth: usize,
    ignore_stack: &mut IgnoreStack,
    state: &mut WalkState<'_>,
) {
    if state.truncated {
        return;
    }

    state.directories_visited += 1;
    if state.directories_visited > state.limits.max_directories {
        state.truncated = true;
        return;
    }

    let entries = match read_dir_sorted(dir_path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    // Collect subdirectories to recurse into after processing all entries.
    let mut subdirs: Vec<(PathBuf, String)> = Vec::new();

    for entry in &entries {
        if state.truncated {
            break;
        }

        state.entries_visited += 1;
        if state.entries_visited > state.limits.max_entries {
            state.truncated = true;
            break;
        }

        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        let relative_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        let metadata = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = metadata.is_dir();
        let is_symlink = metadata.is_symlink();

        // Don't follow symlinks to directories.
        if is_symlink && is_dir {
            if !state.tracked_paths.contains(&relative_path) {
                let classification = match ignore_stack.is_ignored(&relative_path, true) {
                    IgnoreMatch::Ignored { .. } => UntrackedClassification::Ignored,
                    _ => UntrackedClassification::Untracked,
                };
                state.untracked.push(UntrackedEntry {
                    path: relative_path,
                    classification,
                    is_directory: true,
                });
            }
            continue;
        }

        if is_dir {
            match ignore_stack.is_ignored(&relative_path, true) {
                IgnoreMatch::Ignored { .. } => {
                    // Prune: don't descend into ignored directories.
                    if !state.tracked_dirs.contains(&relative_path) {
                        state.untracked.push(UntrackedEntry {
                            path: relative_path,
                            classification: UntrackedClassification::Ignored,
                            is_directory: true,
                        });
                    }
                }
                _ => {
                    if !state.tracked_dirs.contains(&relative_path) {
                        state.untracked.push(UntrackedEntry {
                            path: relative_path.clone(),
                            classification: UntrackedClassification::Untracked,
                            is_directory: true,
                        });
                    }
                    subdirs.push((entry.path(), relative_path));
                }
            }
        } else {
            if state.tracked_paths.contains(&relative_path) {
                continue;
            }
            let classification = match ignore_stack.is_ignored(&relative_path, false) {
                IgnoreMatch::Ignored { .. } => UntrackedClassification::Ignored,
                _ => UntrackedClassification::Untracked,
            };
            state.untracked.push(UntrackedEntry {
                path: relative_path,
                classification,
                is_directory: false,
            });
        }
    }

    // Recurse into subdirectories. Parent ignore rules remain on the stack
    // because we haven't returned from this function yet.
    for (subdir_path, subdir_prefix) in subdirs {
        if state.truncated {
            break;
        }
        let child_depth = depth + 1;
        if child_depth > state.limits.max_depth {
            state.truncated = true;
            break;
        }
        ignore_stack.push_directory(&subdir_path);
        walk_directory(&subdir_path, &subdir_prefix, child_depth, ignore_stack, state);
        ignore_stack.pop_directory();
    }
}

/// Read a directory and return its entries sorted by name.
fn read_dir_sorted(path: &Path) -> std::io::Result<Vec<std::fs::DirEntry>> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_directory_prefixes_from_paths() {
        let mut tracked = BTreeSet::new();
        tracked.insert("src/lib/foo.rs".to_owned());
        tracked.insert("README.md".to_owned());
        tracked.insert("src/main.rs".to_owned());

        let dirs = tracked_directory_prefixes(&tracked);
        assert!(dirs.contains(""));
        assert!(dirs.contains("src"));
        assert!(dirs.contains("src/lib"));
        assert!(!dirs.contains("src/lib/foo.rs"));
        assert!(!dirs.contains("README.md"));
    }

    #[test]
    fn empty_tracked_paths() {
        let tracked = BTreeSet::new();
        let dirs = tracked_directory_prefixes(&tracked);
        assert_eq!(dirs.len(), 1);
        assert!(dirs.contains(""));
    }
}
