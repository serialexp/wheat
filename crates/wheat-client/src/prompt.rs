//! Yes/no questions for a person at the terminal.

use std::io::{BufRead, IsTerminal as _, Write};

use anyhow::Result;

use crate::outcome::OutputFormat;

/// Where a confirmation prompt reads and writes.
pub struct PromptIo<'a> {
    pub input: &'a mut dyn BufRead,
    pub diagnostics: &'a mut dyn Write,
    /// A person can answer: stdin and stderr are terminals and the output is not JSON.
    pub interactive: bool,
}

impl PromptIo<'_> {
    /// Write `question` and read the answer. Only `y` or `yes` (any case) is a yes.
    pub fn ask_yes_no(&mut self, question: &str) -> Result<bool> {
        write!(self.diagnostics, "{question} [y/N] ")?;
        self.diagnostics.flush()?;
        let mut answer = String::new();
        self.input.read_line(&mut answer)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
}

/// Run `f` with a prompt on this process's stdin and stderr. It is interactive
/// only when both are terminals and `format` is human output.
pub fn with_terminal<T>(format: OutputFormat, f: impl FnOnce(&mut PromptIo<'_>) -> T) -> T {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    let interactive = format == OutputFormat::Human && stdin.is_terminal() && stderr.is_terminal();
    f(&mut PromptIo {
        input: &mut stdin.lock(),
        diagnostics: &mut stderr.lock(),
        interactive,
    })
}
