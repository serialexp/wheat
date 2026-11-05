//! The repository's own settings: `.wheat.json` at the workspace root. It is tracked
//! like any other file, so every clone shares it, unlike `<store>/config.json`, which
//! stays with one store.
//!
//! ```json
//! { "format": "wheat-repository", "version": 1, "max_file_bytes": 4294967296 }
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use wheat_core::{FileKind, MAX_CHUNKED_CONTENT_BYTES};

use crate::{
    outcome::CliFailure,
    workspace_files::{WorkspaceFileError, read_workspace_file},
};

/// Where the settings live, relative to the workspace root.
pub const REPOSITORY_CONFIG_PATH: &str = ".wheat.json";
const FORMAT: &str = "wheat-repository";
const VERSION: u32 = 1;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// The longest file `wheat add` captures unless `.wheat.json` says otherwise: a guard
/// against adding a disk image by mistake, not a limit of the format.
pub const DEFAULT_MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
/// The longest file the format stores, and so the most `max_file_bytes` may be.
pub const FORMAT_MAX_FILE_BYTES: usize = {
    assert!(MAX_CHUNKED_CONTENT_BYTES <= usize::MAX as u64);
    MAX_CHUNKED_CONTENT_BYTES as usize
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryConfig {
    format: String,
    version: u32,
    #[serde(default)]
    max_file_bytes: Option<u64>,
}

/// The settings in effect, with defaults for what `.wheat.json` leaves out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositorySettings {
    /// The longest file `wheat add` captures.
    pub max_file_bytes: usize,
}

impl Default for RepositorySettings {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// Reads `.wheat.json` from the workspace at `root` as it is on disk, so raising a
/// limit and adding the file it is for can be one change. Without the file, the
/// defaults apply.
pub fn read(root: &Path) -> Result<RepositorySettings> {
    let invalid =
        |reason: String| CliFailure::invalid(format!("{REPOSITORY_CONFIG_PATH}: {reason}"));
    let snapshot =
        match read_workspace_file(root, Path::new(REPOSITORY_CONFIG_PATH), MAX_CONFIG_BYTES) {
            Ok(None) => return Ok(RepositorySettings::default()),
            Ok(Some(snapshot)) => snapshot,
            Err(WorkspaceFileError::TooLarge { limit }) => {
                return Err(invalid(format!("longer than {limit} bytes")));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {REPOSITORY_CONFIG_PATH}"));
            }
        };
    if snapshot.kind == FileKind::Symlink {
        return Err(invalid("must be a file, not a symlink".into()));
    }
    let config: RepositoryConfig =
        serde_json::from_slice(&snapshot.bytes).map_err(|error| invalid(error.to_string()))?;
    if config.format != FORMAT || config.version != VERSION {
        return Err(invalid(format!(
            "expected format {FORMAT:?} version {VERSION}, found {:?} version {}",
            config.format, config.version
        )));
    }
    let max_file_bytes = match config.max_file_bytes {
        None => DEFAULT_MAX_FILE_BYTES,
        Some(bytes) if bytes <= MAX_CHUNKED_CONTENT_BYTES => usize::try_from(bytes)?,
        Some(bytes) => {
            return Err(invalid(format!(
                "max_file_bytes {bytes} is more than the {MAX_CHUNKED_CONTENT_BYTES} bytes \
                 a file may hold"
            )));
        }
    };
    Ok(RepositorySettings { max_file_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_text(text: &str) -> Result<RepositorySettings> {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(REPOSITORY_CONFIG_PATH), text).unwrap();
        read(root.path())
    }

    #[test]
    fn without_the_file_the_defaults_apply() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            read(root.path()).unwrap(),
            RepositorySettings {
                max_file_bytes: DEFAULT_MAX_FILE_BYTES
            }
        );
        assert_eq!(
            read_text(r#"{"format":"wheat-repository","version":1}"#).unwrap(),
            RepositorySettings::default()
        );
    }

    #[test]
    fn the_file_limit_goes_up_to_the_format_limit() {
        let settings =
            read_text(r#"{"format":"wheat-repository","version":1,"max_file_bytes":4294967296}"#)
                .unwrap();
        assert_eq!(settings.max_file_bytes, 4 << 30);
        let at_most = format!(
            r#"{{"format":"wheat-repository","version":1,"max_file_bytes":{MAX_CHUNKED_CONTENT_BYTES}}}"#
        );
        assert_eq!(
            read_text(&at_most).unwrap().max_file_bytes,
            FORMAT_MAX_FILE_BYTES
        );
        let over = format!(
            r#"{{"format":"wheat-repository","version":1,"max_file_bytes":{}}}"#,
            MAX_CHUNKED_CONTENT_BYTES + 1
        );
        let error = read_text(&over).unwrap_err().to_string();
        assert!(error.contains("max_file_bytes"), "{error}");
    }

    #[test]
    fn a_malformed_file_is_refused() {
        for text in [
            "not json",
            r#"{"format":"wheat-config","version":1}"#,
            r#"{"format":"wheat-repository","version":2}"#,
            r#"{"format":"wheat-repository","version":1,"unknown":true}"#,
        ] {
            let error = read_text(text).unwrap_err().to_string();
            assert!(error.starts_with(".wheat.json: "), "{text}: {error}");
        }
    }
}
