//! Hierarchical ignore-file stacking with precedence rules.
//!
//! An `IgnoreStack` manages layered ignore files from the workspace root down
//! to the current directory. Deeper directories take precedence over shallower
//! ones, and within a single file the last matching line wins. Negation
//! patterns (`!`) re-include previously excluded paths.

use std::path::{Path, PathBuf};

use crate::pattern::{IgnorePattern, parse_ignore_file};

/// Which source produced a particular ignore match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IgnoreSource {
    /// A `.wheatignore` file at the given path.
    Wheatignore(PathBuf),
    /// A `.gitignore` file used as fallback (no sibling `.wheatignore`).
    GitignoreFallback(PathBuf),
    /// The user-global ignore file.
    UserGlobal(PathBuf),
    /// A command-line `--exclude` pattern.
    CommandExclude,
    /// A built-in rule (e.g. `.wheat` directory). Cannot be negated.
    Builtin,
}

/// Result of checking whether a path is ignored.
#[derive(Clone, Debug)]
pub enum IgnoreMatch {
    /// No ignore pattern matched.
    NotIgnored,
    /// Matched by an ignore pattern.
    Ignored {
        source: IgnoreSource,
        line_number: u32,
    },
    /// Matched by a negation pattern (explicitly un-ignored).
    Negated,
}

impl IgnoreMatch {
    pub fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored { .. })
    }
}

/// One layer of ignore patterns from a single source file or the command line.
struct IgnoreLayer {
    patterns: Vec<IgnorePattern>,
    source: IgnoreSource,
    /// How many layers deep this is (for push/pop bookkeeping).
    depth: usize,
}

/// Hierarchical stack of ignore files.
///
/// Layers are pushed as directories are entered and popped when they are left.
/// The bottom layers (user-global, command-line excludes, built-ins) persist
/// for the entire traversal.
pub struct IgnoreStack {
    layers: Vec<IgnoreLayer>,
    /// Current directory depth (0 = workspace root).
    current_depth: usize,
    /// Paths of `.gitignore` files used as fallback during this stack's lifetime.
    gitignore_fallbacks: Vec<PathBuf>,
    /// Built-in top-level patterns that are always applied and cannot be negated.
    builtin_patterns: Vec<IgnorePattern>,
}

impl IgnoreStack {
    /// Create a new stack.
    ///
    /// `global_ignore` is the optional user-global ignore file content.
    /// `global_path` is its filesystem path (for diagnostics).
    /// `excludes` are command-line `--exclude` patterns.
    pub fn new(global_ignore: Option<(&str, PathBuf)>, excludes: &[String]) -> Self {
        let mut layers = Vec::new();

        // User-global ignore (lowest precedence of the persistent layers).
        if let Some((content, path)) = global_ignore {
            let patterns = parse_ignore_file(content, 0);
            if !patterns.is_empty() {
                layers.push(IgnoreLayer {
                    patterns,
                    source: IgnoreSource::UserGlobal(path),
                    depth: 0,
                });
            }
        }

        // Command-line excludes (highest precedence of the persistent layers).
        if !excludes.is_empty() {
            let combined = excludes.join("\n");
            let patterns = parse_ignore_file(&combined, 0);
            if !patterns.is_empty() {
                layers.push(IgnoreLayer {
                    patterns,
                    source: IgnoreSource::CommandExclude,
                    depth: 0,
                });
            }
        }

        // Built-in patterns: always ignore wheat control paths at top level.
        let builtin_content: String = crate::WORKSPACE_CONTROL_NAMES
            .iter()
            .map(|name| format!("/{name}\n"))
            .collect();
        let builtin_patterns = parse_ignore_file(&builtin_content, 0);

        IgnoreStack {
            layers,
            current_depth: 0,
            gitignore_fallbacks: Vec::new(),
            builtin_patterns,
        }
    }

    /// Push ignore rules for a directory being entered during traversal.
    ///
    /// Looks for `.wheatignore` first; falls back to `.gitignore` if no
    /// `.wheatignore` exists. Records the fallback for diagnostics.
    ///
    /// `dir_path` is the absolute filesystem path of the directory.
    pub fn push_directory(&mut self, dir_path: &Path) {
        self.current_depth += 1;

        let wheatignore = dir_path.join(".wheatignore");
        let gitignore = dir_path.join(".gitignore");

        let (content, source) = if let Ok(content) = std::fs::read_to_string(&wheatignore) {
            (content, IgnoreSource::Wheatignore(wheatignore))
        } else if let Ok(content) = std::fs::read_to_string(&gitignore) {
            self.gitignore_fallbacks.push(gitignore.clone());
            (content, IgnoreSource::GitignoreFallback(gitignore))
        } else {
            return; // No ignore file in this directory.
        };

        let patterns = parse_ignore_file(&content, self.layers.len());
        if !patterns.is_empty() {
            self.layers.push(IgnoreLayer {
                patterns,
                source,
                depth: self.current_depth,
            });
        }
    }

    /// Pop all layers for the most recently pushed directory.
    pub fn pop_directory(&mut self) {
        assert!(self.current_depth > 0, "cannot pop below root depth");
        // Remove all layers at the current depth.
        self.layers.retain(|layer| layer.depth < self.current_depth);
        self.current_depth -= 1;
    }

    /// Check whether a path is ignored.
    ///
    /// `relative_path` is relative to the workspace root, using `/` separators.
    /// `is_dir` must be `true` when the path names a directory.
    pub fn is_ignored(&self, relative_path: &str, is_dir: bool) -> IgnoreMatch {
        // Built-in patterns are checked first and cannot be negated.
        for pattern in &self.builtin_patterns {
            if pattern.matches(relative_path, is_dir) {
                return IgnoreMatch::Ignored {
                    source: IgnoreSource::Builtin,
                    line_number: pattern.line_number,
                };
            }
        }

        // Walk layers from deepest (most specific) to shallowest.
        // Within each layer, walk patterns from last to first (last match wins).
        for layer in self.layers.iter().rev() {
            for pattern in layer.patterns.iter().rev() {
                if pattern.matches(relative_path, is_dir) {
                    if pattern.negation {
                        return IgnoreMatch::Negated;
                    }
                    return IgnoreMatch::Ignored {
                        source: layer.source.clone(),
                        line_number: pattern.line_number,
                    };
                }
            }
        }

        IgnoreMatch::NotIgnored
    }

    /// Return the list of `.gitignore` files used as fallbacks during traversal.
    pub fn gitignore_fallbacks(&self) -> &[PathBuf] {
        &self.gitignore_fallbacks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_stack(excludes: &[String]) -> IgnoreStack {
        IgnoreStack::new(None, excludes)
    }

    #[test]
    fn builtin_rules_always_apply() {
        let stack = make_stack(&[]);
        assert!(stack.is_ignored(".wheat", true).is_ignored());
        assert!(stack.is_ignored(".wheat-git", true).is_ignored());
        assert!(stack.is_ignored("repository.head", false).is_ignored());
        assert!(stack.is_ignored("repository.lock", false).is_ignored());
        assert!(stack.is_ignored("storage", true).is_ignored());
        // But not in subdirectories (anchored with leading /).
        assert!(!stack.is_ignored("sub/.wheat", true).is_ignored());
    }

    #[test]
    fn command_line_excludes() {
        let stack = make_stack(&["*.log".to_owned(), "temp/".to_owned()]);
        assert!(stack.is_ignored("debug.log", false).is_ignored());
        assert!(stack.is_ignored("sub/error.log", false).is_ignored());
        assert!(stack.is_ignored("temp", true).is_ignored());
        assert!(!stack.is_ignored("temp", false).is_ignored());
        assert!(!stack.is_ignored("README.md", false).is_ignored());
    }

    #[test]
    fn global_ignore() {
        let stack = IgnoreStack::new(
            Some((
                "*.bak\n*.swp",
                PathBuf::from("/home/user/.config/wheat/ignore"),
            )),
            &[],
        );
        assert!(stack.is_ignored("file.bak", false).is_ignored());
        assert!(stack.is_ignored("dir/file.swp", false).is_ignored());
        assert!(!stack.is_ignored("file.txt", false).is_ignored());
    }

    #[test]
    fn wheatignore_file_in_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".wheatignore"), "*.log\n").unwrap();

        let mut stack = make_stack(&[]);
        stack.push_directory(root);

        assert!(stack.is_ignored("debug.log", false).is_ignored());
        assert!(!stack.is_ignored("main.rs", false).is_ignored());
        assert!(stack.gitignore_fallbacks().is_empty());

        stack.pop_directory();
        // After pop, the patterns from that directory are gone.
        assert!(!stack.is_ignored("debug.log", false).is_ignored());
    }

    #[test]
    fn gitignore_fallback_when_no_wheatignore() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Only .gitignore, no .wheatignore.
        fs::write(root.join(".gitignore"), "target/\nnode_modules/\n").unwrap();

        let mut stack = make_stack(&[]);
        stack.push_directory(root);

        assert!(stack.is_ignored("target", true).is_ignored());
        assert!(stack.is_ignored("node_modules", true).is_ignored());
        assert_eq!(stack.gitignore_fallbacks().len(), 1);

        stack.pop_directory();
    }

    #[test]
    fn wheatignore_replaces_gitignore() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Both exist; .wheatignore should be used, .gitignore ignored.
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join(".wheatignore"), "*.tmp\n").unwrap();

        let mut stack = make_stack(&[]);
        stack.push_directory(root);

        assert!(!stack.is_ignored("debug.log", false).is_ignored());
        assert!(stack.is_ignored("scratch.tmp", false).is_ignored());
        assert!(stack.gitignore_fallbacks().is_empty());

        stack.pop_directory();
    }

    #[test]
    fn nested_wheatignore_overrides_parent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".wheatignore"), "*.txt\n").unwrap();

        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join(".wheatignore"), "!important.txt\n").unwrap();

        let mut stack = make_stack(&[]);
        stack.push_directory(root);
        assert!(stack.is_ignored("sub/important.txt", false).is_ignored());

        stack.push_directory(&sub);
        // The nested file negates *.txt for important.txt.
        assert!(matches!(
            stack.is_ignored("sub/important.txt", false),
            IgnoreMatch::Negated
        ));

        stack.pop_directory();
        stack.pop_directory();
    }

    #[test]
    fn negation_un_ignores() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".wheatignore"), "*.txt\n!README.txt\n").unwrap();

        let mut stack = make_stack(&[]);
        stack.push_directory(root);

        assert!(stack.is_ignored("notes.txt", false).is_ignored());
        assert!(matches!(
            stack.is_ignored("README.txt", false),
            IgnoreMatch::Negated
        ));

        stack.pop_directory();
    }
}
