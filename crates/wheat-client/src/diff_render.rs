//! Atomic, bounded human and JSON rendering for file diffs.

use std::io::{self, Write};

use anyhow::Result;
use wheat_core::diff_model::{
    DiffDocument, DiffLineOperation, DiffOmissionReason, DiffPresentation, DiffText,
};

use crate::outcome::CliFailure;

pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub fn human_bytes(document: &DiffDocument) -> Result<Vec<u8>> {
    render_human(document)
}

fn render_human(document: &DiffDocument) -> Result<Vec<u8>> {
    render_human_with_limit(document, MAX_OUTPUT_BYTES)
}

fn render_human_with_limit(document: &DiffDocument, limit: usize) -> Result<Vec<u8>> {
    let mut output = LimitedBuffer::new(limit);
    let result = render_human_into(document, &mut output);
    if output.limit_exceeded {
        return Err(CliFailure::limit(
            "diff output exceeded configured byte limit",
        ));
    }
    result?;
    Ok(output.into_inner())
}

fn render_human_into(document: &DiffDocument, output: &mut LimitedBuffer) -> Result<()> {
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
                render_text(&mut *output, diff)?;
            }
            DiffPresentation::Binary => match (&file.old, &file.new) {
                (Some(old), Some(new)) => writeln!(
                    output,
                    "Binary files differ ({} -> {})",
                    old.content, new.content
                )?,
                (None, Some(new)) => writeln!(output, "Binary file added ({})", new.content)?,
                (Some(old), None) => writeln!(output, "Binary file deleted ({})", old.content)?,
                (None, None) => {
                    return Err(CliFailure::corrupt("diff file has neither side"));
                }
            },
            DiffPresentation::Symlink => match (&file.old, &file.new) {
                (Some(old), Some(new)) => writeln!(
                    output,
                    "Symlink target changed ({} -> {})",
                    old.content, new.content
                )?,
                (None, Some(new)) => writeln!(output, "Symlink added ({})", new.content)?,
                (Some(old), None) => writeln!(output, "Symlink deleted ({})", old.content)?,
                (None, None) => {
                    return Err(CliFailure::corrupt("diff file has neither side"));
                }
            },
            DiffPresentation::Kind { diff } => {
                let (Some(old), Some(new)) = (&file.old, &file.new) else {
                    return Err(CliFailure::corrupt(
                        "kind-change diff does not contain both sides",
                    ));
                };
                writeln!(
                    output,
                    "File kind changed ({:?} {} -> {:?} {})",
                    old.kind, old.content, new.kind, new.content
                )?;
                if let Some(diff) = diff {
                    render_text(&mut *output, diff)?;
                }
            }
            DiffPresentation::Omitted { reason } => {
                writeln!(output, "Text diff omitted ({})", omission(*reason))?
            }
        }
    }
    Ok(())
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
            let Some(text) = diff.text.get(line.text_start..line.text_end) else {
                return Err(CliFailure::corrupt(
                    "diff line range is outside authenticated text",
                ));
            };
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
    limit_exceeded: bool,
}
impl LimitedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            limit_exceeded: false,
        }
    }
    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(end) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("diff output size overflow"));
        };
        if end > self.limit {
            self.limit_exceeded = true;
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
    use super::{LimitedBuffer, render_human_with_limit};
    use crate::outcome::CliFailure;
    use std::io::Write;
    use wheat_core::{
        ContentId, FileKind,
        diff_model::{
            DiffComparison, DiffDescriptor, DiffDocument, DiffHunk, DiffLine, DiffLineOperation,
            DiffPresentation, DiffText, FileChangeKind, FileDiff,
        },
    };

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
    fn renderer_returns_typed_output_limit_failure() {
        let descriptor = DiffDescriptor {
            kind: FileKind::Regular,
            content: ContentId::from_digest([1; 32]),
            bytes: 1,
            text: Some(false),
        };
        let document = DiffDocument {
            schema_version: 1,
            comparison: DiffComparison::WorkspaceVsPinned {
                materialized_view: wheat_core::ViewId::from_digest([2; 32]),
                materialized_head_generation: 1,
                current_head_generation: 1,
                authority_advanced: false,
            },
            files: vec![FileDiff {
                path: "path".into(),
                change: FileChangeKind::Added,
                old: None,
                new: Some(descriptor),
                content_changed: true,
                kind_changed: false,
                presentation: DiffPresentation::Binary,
                truncated: false,
            }],
        };

        let error = render_human_with_limit(&document, 1).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CliFailure>(),
            Some(CliFailure::LimitExceeded(_))
        ));
    }

    #[test]
    fn renderer_returns_typed_corruption_for_invalid_text_ranges() {
        let descriptor = DiffDescriptor {
            kind: FileKind::Regular,
            content: ContentId::from_digest([1; 32]),
            bytes: 2,
            text: Some(true),
        };
        let document = DiffDocument {
            schema_version: 1,
            comparison: DiffComparison::WorkspaceVsPinned {
                materialized_view: wheat_core::ViewId::from_digest([2; 32]),
                materialized_head_generation: 1,
                current_head_generation: 1,
                authority_advanced: false,
            },
            files: vec![FileDiff {
                path: "path".into(),
                change: FileChangeKind::Modified,
                old: Some(descriptor.clone()),
                new: Some(descriptor),
                content_changed: true,
                kind_changed: false,
                presentation: DiffPresentation::Text {
                    diff: DiffText {
                        text: "ø".into(),
                        hunks: vec![DiffHunk {
                            old_start: 1,
                            old_count: 1,
                            new_start: 1,
                            new_count: 1,
                            lines: vec![DiffLine {
                                operation: DiffLineOperation::Context,
                                old_line: Some(1),
                                new_line: Some(1),
                                text_start: 1,
                                text_end: 2,
                            }],
                        }],
                    },
                },
                truncated: false,
            }],
        };

        let error = render_human_with_limit(&document, 1024).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CliFailure>(),
            Some(CliFailure::CorruptState(_))
        ));
    }
}
