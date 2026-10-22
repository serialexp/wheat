//! Paths inside a `.git` directory.
//!
//! A tree may legitimately hold files under `.git` (test fixtures, a vendored
//! repository), so Wheat does not forbid them. But writing them into a workspace
//! that is also a git checkout can plant hooks or config that git later runs.
//! So materialization lists every such path it is about to write and asks a
//! [`DotGitGate`] first: `--allow-dot-git` lets them through, a person at the
//! terminal is asked, and anything else (a window, or nobody at the terminal) is
//! refused before a file is written.

use anyhow::{Result, bail};

use crate::outcome::{CliFailure, OutputFormat};
use crate::prompt::{self, PromptIo};

/// How many paths a refusal or question lists before summarizing the rest.
const LISTED_PATHS: usize = 20;

/// Whether `path` (a `/`-separated tree path) has a `.git` component, in any
/// letter case: on case-insensitive disks `.GIT` is the same directory.
pub fn is_dot_git_path(path: &str) -> bool {
    path.split('/')
        .any(|part| part.eq_ignore_ascii_case(".git"))
}

/// Decides whether materialization may write paths inside a `.git` directory.
#[derive(Clone, Copy, Debug)]
pub enum DotGitGate {
    /// `--allow-dot-git` was given.
    Allow,
    /// Ask on this process's terminal when there is something to ask; refuse
    /// when nobody can answer. The terminal is only locked while asking.
    Terminal(OutputFormat),
    /// Nobody can be asked, as in a window with no terminal: refuse.
    Refuse,
}

impl DotGitGate {
    /// The gate for a command: `--allow-dot-git` allows, otherwise ask on the terminal.
    pub fn for_command(format: OutputFormat, allow_dot_git: bool) -> Self {
        if allow_dot_git {
            Self::Allow
        } else {
            Self::Terminal(format)
        }
    }

    /// Returns `Ok` when the `.git` paths among `paths` may be written.
    pub fn check<'p>(self, paths: impl IntoIterator<Item = &'p str>) -> Result<()> {
        match self {
            DotGitGate::Allow => Ok(()),
            DotGitGate::Terminal(format) => ask_about(paths, |dot_git| {
                prompt::with_terminal(format, |io| confirm(dot_git, io))
            }),
            DotGitGate::Refuse => ask_about(paths, |dot_git| {
                bail!(CliFailure::dot_git(format!(
                    "{}run `wheat` with --allow-dot-git to write them",
                    listing(dot_git)
                )))
            }),
        }
    }
}

/// Call `ask` with the `.git` paths among `paths`, unless there are none.
fn ask_about<'p>(
    paths: impl IntoIterator<Item = &'p str>,
    ask: impl FnOnce(&[&str]) -> Result<()>,
) -> Result<()> {
    let dot_git: Vec<&str> = paths.into_iter().filter(|p| is_dot_git_path(p)).collect();
    if dot_git.is_empty() {
        return Ok(());
    }
    ask(&dot_git)
}

/// List the `.git` paths, then ask; refuse when nobody can answer.
fn confirm(dot_git: &[&str], io: &mut PromptIo<'_>) -> Result<()> {
    let listing = listing(dot_git);
    if !io.interactive {
        bail!(CliFailure::dot_git(format!(
            "{listing}rerun with --allow-dot-git to write them"
        )));
    }
    write!(io.diagnostics, "{listing}")?;
    if io.ask_yes_no("Write them?")? {
        Ok(())
    } else {
        bail!(CliFailure::dot_git("cancelled; nothing was written"))
    }
}

fn listing(paths: &[&str]) -> String {
    let mut text = format!(
        "this would write {} path(s) inside a .git directory; \
         if this workspace is also a git checkout, git may run or read them:\n",
        paths.len()
    );
    for path in paths.iter().take(LISTED_PATHS) {
        text.push_str("  ");
        text.push_str(path);
        text.push('\n');
    }
    if paths.len() > LISTED_PATHS {
        text.push_str(&format!("  ... and {} more\n", paths.len() - LISTED_PATHS));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_git_components_match_at_any_depth_and_case() {
        for path in [
            ".git",
            ".git/config",
            ".GIT/hooks/pre-commit",
            "vendor/lib/.Git/HEAD",
            "a/.git",
        ] {
            assert!(is_dot_git_path(path), "{path}");
        }
        for path in [
            ".gitignore",
            ".github/workflows/ci.yml",
            "git/config",
            "a.git/b",
            "x/.git-foo",
        ] {
            assert!(!is_dot_git_path(path), "{path}");
        }
    }

    fn ask(answer: &str, interactive: bool, paths: &[&str]) -> (Result<()>, String) {
        let mut input = answer.as_bytes();
        let mut diagnostics = Vec::new();
        let mut io = PromptIo {
            input: &mut input,
            diagnostics: &mut diagnostics,
            interactive,
        };
        let result = ask_about(paths.iter().copied(), |dot_git| confirm(dot_git, &mut io));
        (result, String::from_utf8(diagnostics).unwrap())
    }

    fn refused(result: &Result<()>) -> bool {
        result.as_ref().is_err_and(|error| {
            matches!(
                error.downcast_ref::<CliFailure>(),
                Some(CliFailure::DotGitPaths(_))
            )
        })
    }

    #[test]
    fn paths_outside_dot_git_pass_without_a_question() {
        let (result, prompt) = ask("", false, &["src/main.rs", ".gitignore"]);
        result.unwrap();
        assert!(prompt.is_empty());
    }

    #[test]
    fn allow_passes_everything() {
        DotGitGate::Allow.check([".git/hooks/pre-commit"]).unwrap();
    }

    #[test]
    fn refuse_passes_other_paths_and_refuses_dot_git_ones() {
        DotGitGate::Refuse
            .check(["src/main.rs", ".gitignore"])
            .unwrap();
        let result = DotGitGate::Refuse.check(["ok.txt", ".git/hooks/post-checkout"]);
        assert!(refused(&result));
        let message = result.unwrap_err().to_string();
        assert!(message.contains(".git/hooks/post-checkout"), "{message}");
        assert!(message.contains("--allow-dot-git"), "{message}");
    }

    #[test]
    fn nobody_to_ask_means_refuse_and_name_the_flag() {
        let (result, prompt) = ask("y\n", false, &["ok.txt", ".git/hooks/post-checkout"]);
        assert!(refused(&result));
        assert!(prompt.is_empty(), "nothing is asked: {prompt}");
        let message = result.unwrap_err().to_string();
        assert!(message.contains(".git/hooks/post-checkout"), "{message}");
        assert!(!message.contains("ok.txt"), "{message}");
        assert!(message.contains("--allow-dot-git"), "{message}");
    }

    #[test]
    fn a_person_is_asked_and_only_yes_proceeds() {
        let (result, prompt) = ask("yes\n", true, &[".git/config"]);
        result.unwrap();
        assert!(prompt.contains("  .git/config\n"), "{prompt}");
        assert!(prompt.ends_with("Write them? [y/N] "), "{prompt}");

        for answer in ["\n", "n\n", "whatever\n", ""] {
            let (result, _) = ask(answer, true, &[".git/config"]);
            assert!(refused(&result), "{answer:?} proceeded");
        }
    }

    #[test]
    fn long_listings_are_summarized() {
        let paths: Vec<String> = (0..25).map(|i| format!(".git/objects/{i}")).collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (result, _) = ask("", false, &paths);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("25 path(s)"), "{message}");
        assert!(message.contains("... and 5 more"), "{message}");
    }
}
