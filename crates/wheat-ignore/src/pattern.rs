//! `.gitignore`-compatible pattern parser.
//!
//! Parses individual lines from `.wheatignore` / `.gitignore` files into
//! compiled glob matchers. The syntax follows Git's `gitignore(5)` specification:
//!
//! - Blank lines and lines starting with `#` are ignored.
//! - A leading `!` negates the pattern.
//! - A trailing `/` restricts the pattern to directories only.
//! - A pattern containing `/` (excluding a trailing one) is anchored to the
//!   directory containing the ignore file.
//! - `*` matches anything except `/`; `**` matches zero or more directories.
//! - `?` matches any single character except `/`.
//! - `[abc]` and `[a-z]` are bracket expressions.

use globset::{GlobBuilder, GlobMatcher};

/// One compiled ignore pattern with its metadata.
#[derive(Clone, Debug)]
pub struct IgnorePattern {
    matcher: GlobMatcher,
    pub negation: bool,
    pub directory_only: bool,
    /// Source file index (for diagnostics).
    pub source_index: usize,
    /// One-based line number in the source file.
    pub line_number: u32,
}

impl IgnorePattern {
    /// Test whether `relative_path` matches this pattern.
    ///
    /// `is_dir` must be `true` when the path names a directory. Directory-only
    /// patterns (those originally ending with `/`) match only directories.
    pub fn matches(&self, relative_path: &str, is_dir: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        self.matcher.is_match(relative_path)
    }
}

/// Parse all patterns from one ignore file's content.
///
/// `source_index` identifies the source file for diagnostics.
pub fn parse_ignore_file(content: &str, source_index: usize) -> Vec<IgnorePattern> {
    let mut patterns = Vec::new();
    for (line_number_zero, raw_line) in content.lines().enumerate() {
        let line_number = (line_number_zero as u32) + 1;
        if let Some(pattern) = parse_line(raw_line, source_index, line_number) {
            patterns.push(pattern);
        }
    }
    patterns
}

/// Parse a single line. Returns `None` for blank lines and comments.
fn parse_line(raw: &str, source_index: usize, line_number: u32) -> Option<IgnorePattern> {
    // Strip trailing whitespace unless escaped with backslash.
    let line = strip_trailing_whitespace(raw);
    if line.is_empty() {
        return None;
    }
    // Comment lines.
    if line.starts_with('#') {
        return None;
    }

    // Negation.
    let (negation, line) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else {
        (false, line)
    };

    if line.is_empty() {
        return None;
    }

    // Directory-only (trailing slash).
    let (directory_only, line) = if let Some(rest) = line.strip_suffix('/') {
        (true, rest)
    } else {
        (false, line)
    };

    if line.is_empty() {
        return None;
    }

    // Determine anchoring: a pattern is anchored if it contains `/` (now that
    // the trailing `/` has been stripped). Leading `/` also anchors and is
    // stripped.
    let (anchored, pattern_str) = if let Some(rest) = line.strip_prefix('/') {
        (true, rest)
    } else {
        let contains_slash = line.contains('/');
        (contains_slash, line)
    };

    if pattern_str.is_empty() {
        return None;
    }

    // Build the glob string.
    // Unanchored patterns match at any depth: prepend `**/`.
    // Anchored patterns match only relative to the ignore file's directory.
    let glob_string = if anchored {
        pattern_str.to_owned()
    } else {
        format!("**/{pattern_str}")
    };

    // Compile the glob with literal_separator so `*` doesn't cross `/`.
    let glob = match GlobBuilder::new(&glob_string)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
    {
        Ok(g) => g,
        Err(_) => return None, // Skip malformed patterns silently.
    };

    Some(IgnorePattern {
        matcher: glob.compile_matcher(),
        negation,
        directory_only,
        source_index,
        line_number,
    })
}

/// Strip trailing unescaped whitespace from a gitignore line.
///
/// Git treats a trailing `\ ` as an escaped space that is part of the pattern.
/// We preserve bytes before any `\ ` sequences at the end.
fn strip_trailing_whitespace(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    // Walk backwards past unescaped whitespace.
    while end > 0 && bytes[end - 1] == b' ' {
        // Check if this space is escaped.
        if end >= 2 && bytes[end - 2] == b'\\' {
            break;
        }
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern_line: &str, path: &str, is_dir: bool) -> bool {
        let patterns = parse_ignore_file(pattern_line, 0);
        assert_eq!(
            patterns.len(),
            1,
            "expected exactly one pattern from '{pattern_line}'"
        );
        patterns[0].matches(path, is_dir)
    }

    #[test]
    fn blank_and_comment_lines() {
        assert!(parse_ignore_file("", 0).is_empty());
        assert!(parse_ignore_file("   ", 0).is_empty());
        assert!(parse_ignore_file("# comment", 0).is_empty());
        // Leading whitespace before `#` makes it a pattern, not a comment.
        // Git treats `#` as a comment only at column 0.
        assert_eq!(parse_ignore_file("  # not a comment", 0).len(), 1);
    }

    #[test]
    fn simple_filename_pattern() {
        assert!(matches("*.txt", "hello.txt", false));
        assert!(matches("*.txt", "dir/hello.txt", false));
        assert!(!matches("*.txt", "hello.rs", false));
    }

    #[test]
    fn directory_only_pattern() {
        // `build/` should only match directories.
        assert!(matches("build/", "build", true));
        assert!(!matches("build/", "build", false));
        // Unanchored, so matches at any depth.
        assert!(matches("build/", "src/build", true));
    }

    #[test]
    fn anchored_pattern_with_leading_slash() {
        // `/config` matches only at root.
        assert!(matches("/config", "config", false));
        assert!(!matches("/config", "sub/config", false));
    }

    #[test]
    fn anchored_pattern_with_internal_slash() {
        // `src/main.rs` is anchored because it contains `/`.
        assert!(matches("src/main.rs", "src/main.rs", false));
        assert!(!matches("src/main.rs", "other/src/main.rs", false));
    }

    #[test]
    fn double_star_prefix() {
        assert!(matches("**/logs", "logs", true));
        assert!(matches("**/logs", "a/b/logs", true));
    }

    #[test]
    fn double_star_middle() {
        assert!(matches("a/**/b", "a/b", false));
        assert!(matches("a/**/b", "a/x/b", false));
        assert!(matches("a/**/b", "a/x/y/b", false));
        assert!(!matches("a/**/b", "c/a/x/b", false));
    }

    #[test]
    fn negation() {
        let patterns = parse_ignore_file("*.txt\n!important.txt", 0);
        assert_eq!(patterns.len(), 2);
        assert!(!patterns[0].negation);
        assert!(patterns[1].negation);
        assert!(patterns[0].matches("foo.txt", false));
        assert!(patterns[1].matches("important.txt", false));
    }

    #[test]
    fn question_mark() {
        assert!(matches("?.txt", "a.txt", false));
        assert!(!matches("?.txt", "ab.txt", false));
        assert!(!matches("?.txt", ".txt", false));
    }

    #[test]
    fn bracket_expression() {
        assert!(matches("[abc].txt", "a.txt", false));
        assert!(matches("[abc].txt", "b.txt", false));
        assert!(!matches("[abc].txt", "d.txt", false));
    }

    #[test]
    fn trailing_whitespace_stripped() {
        // "*.txt   " should match as "*.txt".
        assert!(matches("*.txt   ", "hello.txt", false));
    }

    #[test]
    fn escaped_trailing_space() {
        // "*.txt\\ " — the trailing space is escaped and part of the pattern.
        let patterns = parse_ignore_file("foo\\ ", 0);
        assert_eq!(patterns.len(), 1);
        // The pattern text is "foo\ " (with the escaped space).
        assert!(patterns[0].matches("foo ", false));
    }

    #[test]
    fn line_numbers_are_correct() {
        let patterns = parse_ignore_file("# comment\n\n*.txt\nbuild/", 0);
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].line_number, 3);
        assert_eq!(patterns[1].line_number, 4);
    }

    #[test]
    fn star_does_not_cross_slash() {
        // `*.txt` should not match `dir/file.txt` via the `*` alone — it
        // matches because unanchored patterns get `**/` prepended.
        // But `dir*` should not match `dir/sub`.
        assert!(!matches("/dir*", "dir/sub", false));
    }
}
