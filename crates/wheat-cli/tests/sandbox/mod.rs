//! A workspace directory with room beside it for the test's stores and
//! configuration.
//!
//! Several harnesses keep a workspace's stores and configuration next to it, in
//! its parent directory. The workspace therefore can't be a temporary directory
//! itself: its parent would be the system's temporary directory, and they would
//! outlive the test there.

use std::path::{Path, PathBuf};

pub struct Sandbox {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
}

impl Sandbox {
    /// The workspace directory, empty until the test fills it.
    pub fn path(&self) -> &Path {
        &self.workspace
    }
}

/// A new temporary directory holding an empty `workspace` directory.
pub fn sandbox() -> Sandbox {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    Sandbox {
        _directory: directory,
        workspace,
    }
}
