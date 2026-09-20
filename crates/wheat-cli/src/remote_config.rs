//! Remote configuration — persists the origin URL and last-fetched head
//! for divergence tracking across fetch/publish operations.
//!
//! Stored as `remote.json` in the wheat store root, alongside `store.json`
//! and `repository.head`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wheat_core::RepositoryEnvelopeId;
use wheat_store::RepositoryHead;

/// On-disk remote configuration.
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// The wheat:// URL of the origin server.
    pub url: String,
    /// Generation counter of the last-fetched (or cloned/published) remote head.
    pub last_fetched_generation: u64,
    /// Hex-encoded envelope ID of the last-fetched remote head.
    pub last_fetched_envelope: String,
}

impl RemoteConfig {
    /// Build a new remote config from a URL and a head.
    pub fn new(url: &str, head: &RepositoryHead) -> Self {
        Self {
            url: url.to_string(),
            last_fetched_generation: head.generation,
            last_fetched_envelope: head.envelope.to_string(),
        }
    }

    /// The last-fetched head as a `RepositoryHead`.
    pub fn last_fetched_head(&self) -> Result<RepositoryHead> {
        let envelope = self
            .last_fetched_envelope
            .parse::<RepositoryEnvelopeId>()
            .with_context(|| {
                format!(
                    "invalid envelope ID in remote.json: {}",
                    self.last_fetched_envelope
                )
            })?;
        Ok(RepositoryHead {
            generation: self.last_fetched_generation,
            envelope,
        })
    }

    /// Update the last-fetched head.
    pub fn set_last_fetched(&mut self, head: &RepositoryHead) {
        self.last_fetched_generation = head.generation;
        self.last_fetched_envelope = head.envelope.to_string();
    }

    /// Read remote.json from a store root. Returns None if the file doesn't exist.
    pub fn read(store_root: &Path) -> Result<Option<Self>> {
        let path = remote_config_path(store_root);
        if !path.is_file() {
            return Ok(None);
        }
        let data =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Self =
            serde_json::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(config))
    }

    /// Write remote.json to a store root. Overwrites any existing file.
    pub fn write(&self, store_root: &Path) -> Result<()> {
        let path = remote_config_path(store_root);
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

fn remote_config_path(store_root: &Path) -> PathBuf {
    store_root.join("remote.json")
}
