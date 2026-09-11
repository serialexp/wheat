//! Atomic, bounded human and JSON rendering for file diffs.

use std::io::{self, Write};

use anyhow::Result;
use wheat_core::diff_model::{
    DiffDocument, DiffLineOperation, DiffOmissionReason, DiffPresentation, DiffText,
};

pub(crate) const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn human(document: &DiffDocument) -> Result<()> {
    emit(render_human(document, MAX_OUTPUT_BYTES)?)
}
pub(crate) fn json(document: &DiffDocument) -> Result<()> {
    emit(render_json(document, MAX_OUTPUT_BYTES)?)
}

fn emit(bytes: Vec<u8>) -> Result<()> {
    let stdout = io::stdout();
    commit(&mut stdout.lock(), &bytes)
}

fn commit(destination: &mut impl Write, bytes: &[u8]) -> Result<()> {
    // Rendering has completed successfully. Commit with exactly one destination write so an
    // overflow or serialization error can never expose a prefix of the document.
    let written = destination.write(bytes)?;
    if written != bytes.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short diff output write").into());
    }
    Ok(())
}

fn render_human(document: &DiffDocument, limit: usize) -> Result<Vec<u8>> {
    let mut output = LimitedBuffer::new(limit);
    for file in &document.files {
        let old_path = display_path("a", &file.path);
        let new_path = display_path("b", &file.path);
        writeln!(output, "diff --wheat {old_path} {new_path}")?;
        writeln!(
            output,
            "--- {}",
            if file.old.is_some() {
                &old_path
            } else {
                "/dev/null"
            }
        )?;
        writeln!(
            output,
            "+++ {}",
            if file.new.is_some() {
                &new_path
            } else {
                "/dev/null"
            }
        )?;
        match &file.presentation {
            DiffPresentation::Text { diff } => {
                if file.old.is_none() {
                    writeln!(output, "File added")?;
                } else if file.new.is_none() {
                    writeln!(output, "File deleted")?;
                }
                render_text(&mut output, diff)?;
            }
            DiffPresentation::Binary => match (&file.old, &file.new) {
                (Some(old), Some(new)) => writeln!(
                    output,
                    "Binary files differ ({} -> {})",
                    old.content, new.content
                )?,
                (None, Some(new)) => writeln!(output, "Binary file added ({})", new.content)?,
                (Some(old), None) => writeln!(output, "Binary file deleted ({})", old.content)?,
                (None, None) => unreachable!("diff has at least one side"),
            },
            DiffPresentation::Symlink => match (&file.old, &file.new) {
                (Some(old), Some(new)) => writeln!(
                    output,
                    "Symlink target changed ({} -> {})",
                    old.content, new.content
                )?,
                (None, Some(new)) => writeln!(output, "Symlink added ({})", new.content)?,
                (Some(old), None) => writeln!(output, "Symlink deleted ({})", old.content)?,
                (None, None) => unreachable!("diff has at least one side"),
            },
            DiffPresentation::Kind { diff } => {
                let old = file.old.as_ref().expect("kind change old side");
                let new = file.new.as_ref().expect("kind change new side");
                writeln!(
                    output,
                    "File kind changed ({:?} {} -> {:?} {})",
                    old.kind, old.content, new.kind, new.content
                )?;
                if let Some(diff) = diff {
                    render_text(&mut output, diff)?;
                }
            }
            DiffPresentation::Omitted { reason } => {
                writeln!(output, "Text diff omitted ({})", omission(*reason))?
            }
        }
    }
    Ok(output.into_inner())
}

fn render_text(output: &mut impl Write, diff: &DiffText) -> Result<()> {
    for hunk in &diff.hunks {
        writeln!(
            output,
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        )?;
        for line in &hunk.lines {
            let prefix = match line.operation {
                DiffLineOperation::Context => ' ',
                DiffLineOperation::Delete => '-',
                DiffLineOperation::Insert => '+',
            };
            let text = &diff.text[line.text_start..line.text_end];
            write!(output, "{prefix}{text}")?;
            if !text.ends_with('\n') {
                writeln!(output)?;
                writeln!(output, "\\ No newline at end of file")?;
            }
        }
    }
    Ok(())
}

fn display_path(prefix: &str, path: &str) -> String {
    let path = format!("{prefix}/{path}");
    if path
        .chars()
        .all(|character| !character.is_control() && character != '"' && character != '\\')
    {
        path
    } else {
        format!("{path:?}")
    }
}

fn render_json(document: &DiffDocument, limit: usize) -> Result<Vec<u8>> {
    let mut output = LimitedBuffer::new(limit);
    serde_json::to_writer_pretty(&mut output, document)?;
    writeln!(output)?;
    Ok(output.into_inner())
}

fn omission(reason: DiffOmissionReason) -> &'static str {
    match reason {
        DiffOmissionReason::InlineBytes => "inline byte limit",
        DiffOmissionReason::LineCount => "line limit",
        DiffOmissionReason::ModelBytes => "aggregate model limit",
        DiffOmissionReason::WorkBudget => "work budget",
        DiffOmissionReason::Deadline => "deadline",
    }
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl LimitedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("diff output size overflow"))?;
        if end > self.limit {
            return Err(io::Error::other(
                "diff output exceeded configured byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{LimitedBuffer, commit};
    use std::io::{self, Write};

    #[test]
    fn output_limit_is_enforced_without_partial_write() {
        let mut exact = LimitedBuffer::new(3);
        exact.write_all(b"abc").unwrap();
        assert_eq!(exact.into_inner(), b"abc");

        let mut overflow = LimitedBuffer::new(3);
        assert!(overflow.write_all(b"abcd").is_err());
        assert!(overflow.into_inner().is_empty());
    }

    #[test]
    fn successful_output_is_committed_with_one_destination_write() {
        #[derive(Default)]
        struct Counted {
            writes: usize,
            bytes: Vec<u8>,
        }
        impl Write for Counted {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut destination = Counted::default();
        commit(&mut destination, b"complete\n").unwrap();
        assert_eq!(destination.writes, 1);
        assert_eq!(destination.bytes, b"complete\n");
    }
}
