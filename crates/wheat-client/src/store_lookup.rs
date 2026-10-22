use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{remote::WheatUrl, remote_config::RemoteConfig, workspace_metadata::StoreMetadata};

const MAX_STORE_CANDIDATES: usize = 10_000;

pub fn find_remote_store(home: &Path, remote: &WheatUrl) -> Result<Option<PathBuf>> {
    let mut matches = Vec::new();
    let mut count = 0usize;
    for entry in fs::read_dir(home)
        .with_context(|| format!("enumerate Wheat store home {}", home.display()))?
    {
        count = count
            .checked_add(1)
            .context("store candidate count overflow")?;
        ensure!(
            count <= MAX_STORE_CANDIDATES,
            "Wheat store home exceeds the {MAX_STORE_CANDIDATES}-entry lookup limit"
        );
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_dir() || entry.file_type()?.is_symlink() {
            continue;
        }
        let path = entry.path();
        if StoreMetadata::read(&path).is_err() {
            continue;
        }
        let Some(config) = RemoteConfig::read(&path).ok().flatten() else {
            continue;
        };
        if config.has_same_identity(remote) {
            matches.push(path.canonicalize()?);
        }
    }
    matches.sort();
    matches.dedup();
    ensure!(
        matches.len() <= 1,
        "multiple local Wheat stores track {}; use --store to select one",
        remote.canonical_identity()
    );
    Ok(matches.pop())
}
