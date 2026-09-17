//! File I/O for loading ignore rules from disk.

use std::path::{Path, PathBuf};

/// Resolve the user-global ignore file path.
///
/// Returns the path to `$XDG_CONFIG_HOME/wheat/ignore` (defaulting to
/// `~/.config/wheat/ignore`). Does not check whether the file exists.
pub fn global_ignore_path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut p = PathBuf::from(home);
                p.push(".config");
                p
            })
        })?;
    Some(config_home.join("wheat").join("ignore"))
}

/// Try to read the user-global ignore file.
///
/// Returns `Some((content, path))` if the file exists and is readable,
/// `None` otherwise. Errors (permission denied, etc.) are silently ignored
/// because a missing or unreadable global ignore file is not an error.
pub fn read_global_ignore() -> Option<(String, PathBuf)> {
    let path = global_ignore_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, path))
}

/// Try to read an ignore file at the given path.
///
/// Returns `None` if the file does not exist. Propagates other I/O errors.
pub fn read_ignore_file(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
