use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::outcome::CliFailure;
use crate::workspace_metadata::{StoreMetadata, WorkspaceMetadata};

/// The workspace at `workspace`, checked against an explicit store when one was given.
pub fn resolve_context(
    workspace: &Path,
    explicit_store: Option<&Path>,
) -> Result<ResolvedWorkspace> {
    let context = discover_workspace(workspace)?;
    if let Some(store) = explicit_store {
        let explicit = canonical_store_path(store)?;
        if explicit != context.store_root {
            return Err(CliFailure::invalid(
                "explicit Wheat store does not match the discovered workspace link",
            ));
        }
    }
    Ok(context)
}

/// The store an explicit `--store` names, or the one the workspace links to.
pub fn resolve_store_root(workspace: &Path, explicit_store: Option<&Path>) -> Result<PathBuf> {
    if let Some(store) = explicit_store {
        let store = canonical_store_path(store)?;
        StoreMetadata::read(&store)?;
        return Ok(store);
    }
    Ok(discover_workspace(workspace)?.store_root)
}

pub struct ResolvedWorkspace {
    pub root: PathBuf,
    pub store_root: PathBuf,
    pub workspace: WorkspaceMetadata,
}

pub fn store_home() -> Result<PathBuf> {
    let home = if let Some(path) = std::env::var_os("WHEAT_STORE_HOME") {
        PathBuf::from(path)
    } else if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path).join("wheat/stores")
    } else {
        PathBuf::from(
            std::env::var_os("HOME")
                .context("cannot locate Wheat store home: set WHEAT_STORE_HOME or XDG_DATA_HOME")?,
        )
        .join(".local/share/wheat/stores")
    };
    std::fs::create_dir_all(&home)
        .with_context(|| format!("create Wheat store home {}", home.display()))?;
    home.canonicalize()
        .with_context(|| format!("canonicalize Wheat store home {}", home.display()))
}

pub fn allocate_store_root() -> Result<PathBuf> {
    Ok(store_home()?.join(uuid::Uuid::now_v7().to_string()))
}

pub fn canonical_store_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalize Wheat store {}", path.display()))
}

pub fn discover_workspace(start: &Path) -> Result<ResolvedWorkspace> {
    let start = start
        .canonicalize()
        .with_context(|| format!("canonicalize workspace path {}", start.display()))?;
    let start_device = device(&start)?;
    for candidate in start.ancestors() {
        if device(candidate)? != start_device {
            break;
        }
        let control = candidate.join(".wheat");
        match std::fs::symlink_metadata(&control) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "Wheat workspace control path is a symlink: {}",
                    control.display()
                )
            }
            Ok(metadata) if metadata.is_dir() => {
                let workspace = WorkspaceMetadata::read(candidate)?;
                let store_root = canonical_store_path(&workspace.store.path)?;
                let store = StoreMetadata::read(&store_root)?;
                if store.store_id != workspace.store.store_id {
                    bail!(
                        "Wheat workspace store identity does not match {}",
                        store_root.display()
                    );
                }
                return Ok(ResolvedWorkspace {
                    root: candidate.to_path_buf(),
                    store_root,
                    workspace,
                });
            }
            Ok(_) => bail!(
                "invalid Wheat workspace control path: {}",
                control.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    bail!("cannot find a Wheat workspace from {}", start.display())
}

#[cfg(unix)]
fn device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(path)?.dev())
}

#[cfg(not(unix))]
fn device(_path: &Path) -> Result<u64> {
    Ok(0)
}
