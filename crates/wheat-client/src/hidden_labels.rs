//! Hidden labels, those named with a leading dot ([`wheat_core::is_hidden_label`]).
//! They are labels like any other: fetched, published and joined unchanged, so the
//! local head holds them as the remote does. Label listings just leave them out,
//! unless asked to list them all or the remote binding names a glob pattern that
//! shows them (`remote.meta`, set by `wheat clone --label` and `wheat remote labels`).
//! A hidden label named directly works as any label does.

use std::{collections::BTreeSet, path::Path};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::{
    outcome::{CliFailure, Success},
    remote_config::RemoteConfig,
};

/// The most patterns a remote binding keeps.
pub const MAX_SHOWN_PATTERNS: usize = 256;
/// The longest pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 1024;

/// Which labels a listing shows.
pub struct LabelFilter {
    all: bool,
    shown: GlobSet,
}

impl LabelFilter {
    /// Every label not hidden, and the hidden ones `patterns` match; with `all`, every
    /// label.
    pub fn new(patterns: &[String], all: bool) -> Result<Self> {
        Ok(Self {
            all,
            shown: compile(patterns)?,
        })
    }

    /// The filter of the store at `store_root`: the patterns of its remote binding,
    /// none when it has no remote.
    pub fn of_store(store_root: &Path, all: bool) -> Result<Self> {
        let patterns = RemoteConfig::read(store_root)?
            .map(|config| config.shown_hidden_labels)
            .unwrap_or_default();
        Self::new(&patterns, all)
    }

    #[must_use]
    pub fn shows(&self, name: &str) -> bool {
        self.all || !wheat_core::is_hidden_label(name) || self.shown.is_match(name)
    }

    /// The label a new workspace starts on: `main` when there is one, else the first
    /// label shown, else `main` for an empty repository.
    pub fn default_label<'a>(&self, mut names: impl Iterator<Item = &'a String>) -> String {
        let mut first = None;
        for name in names.by_ref() {
            if name == "main" {
                return name.clone();
            }
            if first.is_none() && self.shows(name) {
                first = Some(name.clone());
            }
        }
        first.unwrap_or_else(|| "main".to_owned())
    }
}

/// Checks `patterns` as a remote binding keeps them and compiles them. In a pattern
/// `*` and `?` stay within one `/`-separated component, `**` crosses them, and `\`
/// escapes.
pub fn compile(patterns: &[String]) -> Result<GlobSet> {
    if patterns.len() > MAX_SHOWN_PATTERNS {
        return Err(CliFailure::limit(format!(
            "a remote shows at most {MAX_SHOWN_PATTERNS} hidden label patterns"
        )));
    }
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        set.add(glob(pattern)?);
    }
    set.build().context("compiling the hidden label patterns")
}

fn glob(pattern: &str) -> Result<globset::Glob> {
    if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
        return Err(CliFailure::invalid(format!(
            "a hidden label pattern is 1 to {MAX_PATTERN_BYTES} bytes"
        )));
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|error| CliFailure::invalid(format!("invalid label pattern {pattern:?}: {error}")))
}

#[derive(Debug, Serialize)]
struct ShownDocument<'a> {
    shown_hidden_labels: &'a [String],
}

/// `wheat remote labels`: adds `add` to and removes `remove` from the hidden label
/// patterns the store's remote binding shows, then lists them.
pub fn edit_shown(store_root: &Path, add: &[String], remove: &[String]) -> Result<Success> {
    let config = update_shown(store_root, add, remove)?;
    let mut human = String::new();
    for pattern in &config.shown_hidden_labels {
        human.push_str(pattern);
        human.push('\n');
    }
    Success::new(
        "remote.labels",
        &ShownDocument {
            shown_hidden_labels: &config.shown_hidden_labels,
        },
        human,
    )
}

/// Adds `add` to and removes `remove` from the hidden label patterns the store's
/// remote binding shows, writing the binding when they change. Refuses a pattern to
/// remove that it does not have.
pub fn update_shown(store_root: &Path, add: &[String], remove: &[String]) -> Result<RemoteConfig> {
    let mut config = RemoteConfig::read(store_root)?.ok_or_else(|| {
        CliFailure::invalid(
            "this store tracks no native remote, so it has no hidden labels to show",
        )
    })?;
    for pattern in add {
        glob(pattern)?;
    }
    let mut patterns: BTreeSet<String> = config.shown_hidden_labels.iter().cloned().collect();
    for pattern in remove {
        if !patterns.remove(pattern) {
            return Err(CliFailure::invalid(format!(
                "the remote shows no hidden labels by {pattern:?}"
            )));
        }
    }
    patterns.extend(add.iter().cloned());
    let patterns: Vec<String> = patterns.into_iter().collect();
    if patterns != config.shown_hidden_labels {
        compile(&patterns)?;
        config.shown_hidden_labels = patterns;
        config.write(store_root)?;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(patterns: &[&str], all: bool) -> LabelFilter {
        let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
        LabelFilter::new(&patterns, all).unwrap()
    }

    #[test]
    fn a_listing_shows_hidden_labels_only_by_pattern_or_when_asked() {
        let shown = filter(&[".gothab/pr/*", ".keep/**"], false);
        assert!(shown.shows("main"));
        assert!(shown.shows("feature/x"));
        assert!(shown.shows(".gothab/pr/12"));
        // `*` stays within one component; `**` crosses them.
        assert!(!shown.shows(".gothab/pr/12/merge"));
        assert!(shown.shows(".keep/a/b/c"));
        assert!(!shown.shows(".gothab/other"));
        assert!(filter(&[], true).shows(".gothab/other"));
        assert!(!filter(&[], false).shows(".x"));
    }

    #[test]
    fn a_new_workspace_starts_on_main_else_the_first_label_shown() {
        let names =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).to_owned()).collect() };
        let none = filter(&[], false);
        // Hidden labels sort first, but a workspace never starts on one unasked.
        assert_eq!(
            none.default_label(names(&[".a", "b", "main"]).iter()),
            "main"
        );
        assert_eq!(none.default_label(names(&[".a", "b", "c"]).iter()), "b");
        assert_eq!(none.default_label(names(&[".a"]).iter()), "main");
        assert_eq!(
            filter(&[".a"], false).default_label(names(&[".a", "b"]).iter()),
            ".a"
        );
    }

    #[test]
    fn patterns_are_checked() {
        assert!(compile(&["".to_owned()]).is_err());
        assert!(compile(&["[".to_owned()]).is_err());
        assert!(compile(&["x".repeat(MAX_PATTERN_BYTES + 1)]).is_err());
        assert!(compile(&vec![".a".to_owned(); MAX_SHOWN_PATTERNS + 1]).is_err());
        assert!(compile(&[".gothab/**".to_owned()]).is_ok());
    }
}
