//! Durable remote configuration for divergence tracking across fetch/publish.
//!
//! `remote.meta` lives in the wheat store root beside `store.meta` and
//! `repository.head`. Writes use a same-directory atomic replacement and sync
//! both the replacement and containing directory before returning.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use wheat_core::{
    RemoteConfigRecord, RepositoryEnvelopeId, RepositoryRootRecord, decode_remote_config,
    encode_remote_config,
};
use wheat_store::RepositoryHead;

use wheat_remote::{WheatUrl, parse_wheat_url};

/// The remote binding, in a store's root.
pub const REMOTE_CONFIG_FILE: &str = "remote.meta";
const MAX_REMOTE_CONFIG_BYTES: u64 = 64 * 1024;

/// In-memory remote configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteConfig {
    /// User-facing URL spelling retained for display.
    pub url: String,
    /// Stable, canonical URL identity used for comparisons.
    pub canonical_identity: String,
    /// Generation counter of the exact last-observed remote head.
    pub last_fetched_generation: u64,
    /// Hex-encoded envelope ID of the exact last-observed remote head.
    pub last_fetched_envelope: String,
    /// Glob patterns of the hidden labels label listings show, sorted
    /// ([`crate::hidden_labels`]).
    pub shown_hidden_labels: Vec<String>,
}

impl RemoteConfig {
    /// Build a remote config from a URL and head.
    ///
    /// Callers already validate command-line remotes before reaching this
    /// constructor. `parse` is available where validation must be fallible.
    pub fn new(display_url: &str, head: &RepositoryHead) -> Self {
        Self::parse(display_url, head).expect("RemoteConfig::new requires a valid wheat URL")
    }

    /// Parse a display URL and construct a validated remote config.
    pub fn parse(display_url: &str, head: &RepositoryHead) -> Result<Self> {
        let remote = parse_wheat_url(display_url)?;
        Ok(Self {
            url: display_url.to_owned(),
            canonical_identity: remote.canonical_identity().to_owned(),
            last_fetched_generation: head.generation,
            last_fetched_envelope: head.envelope.to_string(),
            shown_hidden_labels: Vec::new(),
        })
    }

    /// Compare this config to a parsed remote by canonical identity.
    pub fn has_same_identity(&self, remote: &WheatUrl) -> bool {
        self.canonical_identity == remote.canonical_identity()
    }

    #[cfg(test)]
    fn matches_url(&self, url: &str) -> Result<bool> {
        Ok(self.has_same_identity(&parse_wheat_url(url)?))
    }

    /// Replace the display URL and identity from an already validated remote.
    pub fn set_remote(&mut self, display_url: &str, remote: &WheatUrl) {
        self.url = display_url.to_owned();
        self.canonical_identity = remote.canonical_identity().to_owned();
    }

    /// The exact last-fetched head as a `RepositoryHead`.
    pub fn last_fetched_head(&self) -> Result<RepositoryHead> {
        parse_head(self.last_fetched_generation, &self.last_fetched_envelope)
    }

    /// Update the exact last-fetched head.
    pub fn set_last_fetched(&mut self, head: &RepositoryHead) {
        self.last_fetched_generation = head.generation;
        self.last_fetched_envelope = head.envelope.to_string();
    }

    /// Read and validate `remote.meta`. Returns `None` when it does not exist.
    pub fn read(store_root: &Path) -> Result<Option<Self>> {
        let path = remote_config_path(store_root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        };
        if !metadata.file_type().is_file() {
            bail!("remote config is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_REMOTE_CONFIG_BYTES {
            bail!(
                "remote config exceeds {} bytes: {}",
                MAX_REMOTE_CONFIG_BYTES,
                path.display()
            );
        }

        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut data = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_REMOTE_CONFIG_BYTES + 1)
            .read_to_end(&mut data)
            .with_context(|| format!("read {}", path.display()))?;
        if data.len() as u64 > MAX_REMOTE_CONFIG_BYTES {
            bail!(
                "remote config exceeds {} bytes: {}",
                MAX_REMOTE_CONFIG_BYTES,
                path.display()
            );
        }
        let stored =
            decode_remote_config(&data).with_context(|| format!("decode {}", path.display()))?;
        let parsed = parse_wheat_url(&stored.display_url)
            .with_context(|| format!("invalid display URL in {}", path.display()))?;
        if parsed.canonical_identity() != stored.canonical_identity {
            bail!(
                "canonical remote identity does not match display URL in {}",
                path.display()
            );
        }
        if !stored.shown_hidden_labels.is_sorted_by(|a, b| a < b) {
            bail!(
                "hidden label patterns are not sorted and unique in {}",
                path.display()
            );
        }
        crate::hidden_labels::compile(&stored.shown_hidden_labels)
            .with_context(|| format!("invalid hidden label patterns in {}", path.display()))?;
        Ok(Some(Self {
            url: stored.display_url,
            canonical_identity: stored.canonical_identity,
            last_fetched_generation: stored.head.generation,
            last_fetched_envelope: stored.head.envelope.to_string(),
            shown_hidden_labels: stored.shown_hidden_labels,
        }))
    }

    /// Atomically and durably replace `remote.meta`.
    pub fn write(&self, store_root: &Path) -> Result<()> {
        let parsed = parse_wheat_url(&self.url).context("invalid remote display URL")?;
        if !self.has_same_identity(&parsed) {
            bail!("canonical remote identity does not match display URL");
        }
        let head = self.last_fetched_head()?;
        if !self.shown_hidden_labels.is_sorted_by(|a, b| a < b) {
            bail!("hidden label patterns must be sorted and unique");
        }
        crate::hidden_labels::compile(&self.shown_hidden_labels)?;

        let data = encode_remote_config(&RemoteConfigRecord {
            display_url: self.url.clone(),
            canonical_identity: self.canonical_identity.clone(),
            head: RepositoryRootRecord {
                generation: head.generation,
                envelope: head.envelope,
            },
            shown_hidden_labels: self.shown_hidden_labels.clone(),
        })?;
        if data.len() as u64 > MAX_REMOTE_CONFIG_BYTES {
            bail!("encoded remote config exceeds {MAX_REMOTE_CONFIG_BYTES} bytes");
        }

        let path = remote_config_path(store_root);
        let mut temporary = tempfile::NamedTempFile::new_in(store_root)
            .with_context(|| format!("create temporary file in {}", store_root.display()))?;
        temporary
            .write_all(&data)
            .with_context(|| format!("write temporary remote config for {}", path.display()))?;
        temporary
            .as_file_mut()
            .sync_all()
            .with_context(|| format!("sync temporary remote config for {}", path.display()))?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", path.display()))?;
        File::open(store_root)
            .with_context(|| format!("open {} for sync", store_root.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", store_root.display()))?;
        Ok(())
    }
}

fn parse_head(generation: u64, envelope: &str) -> Result<RepositoryHead> {
    let envelope = envelope
        .parse::<RepositoryEnvelopeId>()
        .with_context(|| format!("invalid envelope ID in {REMOTE_CONFIG_FILE}: {envelope}"))?;
    Ok(RepositoryHead {
        generation,
        envelope,
    })
}

/// The remote binding of the store at `store_root`.
pub fn remote_config_path(store_root: &Path) -> PathBuf {
    store_root.join(REMOTE_CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(generation: u64) -> RepositoryHead {
        RepositoryHead {
            generation,
            envelope: RepositoryEnvelopeId::from_digest([generation as u8; 32]),
        }
    }

    #[test]
    fn writes_config_and_round_trips_exact_head() {
        let directory = tempfile::tempdir().unwrap();
        let remote = parse_wheat_url("wheat+tls://EXAMPLE.com/Owner/Repo").unwrap();
        let mut config = RemoteConfig::new("wheat+tls://EXAMPLE.com/Owner/Repo", &head(7));
        config.shown_hidden_labels = vec![".gothab/**".to_owned(), ".pr/*".to_owned()];
        assert!(config.has_same_identity(&remote));
        config.write(directory.path()).unwrap();

        let bytes = fs::read(remote_config_path(directory.path())).unwrap();
        let stored = decode_remote_config(&bytes).unwrap();
        assert_eq!(stored.display_url, "wheat+tls://EXAMPLE.com/Owner/Repo");
        assert_eq!(
            stored.canonical_identity,
            "wheat+tls://example.com/Owner/Repo"
        );
        assert_eq!(stored.head.generation, 7);
        assert_eq!(stored.head.envelope, head(7).envelope);
        assert_eq!(stored.shown_hidden_labels, [".gothab/**", ".pr/*"]);

        let loaded = RemoteConfig::read(directory.path()).unwrap().unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.last_fetched_head().unwrap(), head(7));
        assert!(
            loaded
                .matches_url("wheat+tls://example.COM/Owner/Repo")
                .unwrap()
        );
    }

    #[test]
    fn rejects_other_bytes_identity_mismatch_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = remote_config_path(directory.path());
        fs::write(&path, b"{\"version\":1}").unwrap();
        assert!(RemoteConfig::read(directory.path()).is_err());

        let record = |canonical_identity: &str| {
            encode_remote_config(&RemoteConfigRecord {
                display_url: "wheat://example.com/Owner/Repo".into(),
                canonical_identity: canonical_identity.into(),
                head: RepositoryRootRecord {
                    generation: 1,
                    envelope: head(1).envelope,
                },
                shown_hidden_labels: Vec::new(),
            })
            .unwrap()
        };
        let mut trailing = record("wheat://example.com/Owner/Repo");
        fs::write(&path, &trailing).unwrap();
        assert!(RemoteConfig::read(directory.path()).unwrap().is_some());
        trailing.push(0);
        fs::write(&path, &trailing).unwrap();
        assert!(RemoteConfig::read(directory.path()).is_err());

        fs::write(&path, record("wheat://other.example/Owner/Repo")).unwrap();
        let error = RemoteConfig::read(directory.path()).unwrap_err();
        assert!(
            error.to_string().contains("does not match display URL"),
            "{error:#}"
        );

        fs::write(&path, vec![b' '; MAX_REMOTE_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(RemoteConfig::read(directory.path()).is_err());
    }
}
