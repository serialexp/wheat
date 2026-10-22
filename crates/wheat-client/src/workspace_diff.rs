//! Build the shared bounded diff model from canonical, bounded inputs.

use std::time::Instant;

use anyhow::{Result, ensure};
use similar::{ChangeTag, TextDiff};
use wheat_core::{
    FileKind,
    diff_model::{
        DiffComparison, DiffDescriptor, DiffDocument, DiffHunk, DiffLimits, DiffLine,
        DiffLineOperation, DiffOmissionReason, DiffPresentation, DiffText, FileChangeKind,
        FileDiff,
    },
    text_capture::{self, CaptureKind},
};

use crate::{repository_diff::TreeChangeInput, workspace_scan::WorkspaceScan};

struct Budget {
    text: usize,
    paths: usize,
    lines: usize,
    work: usize,
    deadline: Instant,
    algorithms_exhausted: bool,
}

impl Budget {
    fn new(limits: DiffLimits, deadline: Instant) -> Self {
        Self {
            text: limits.max_model_text_bytes,
            paths: limits.max_model_paths_bytes,
            lines: limits.max_line_entries,
            work: limits.max_work_units,
            deadline,
            algorithms_exhausted: false,
        }
    }

    fn charge_path(&mut self, path: &str) -> Result<()> {
        ensure!(
            path.len() <= self.paths,
            "diff paths exceed aggregate model byte limit"
        );
        self.paths -= path.len();
        Ok(())
    }
}

pub fn build_repository(
    comparison: DiffComparison,
    changes: Vec<TreeChangeInput>,
    limits: DiffLimits,
    deadline: Instant,
) -> Result<DiffDocument> {
    let mut budget = Budget::new(limits, deadline);
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        budget.charge_path(&change.path)?;
        let old = change.old.map(|side| descriptor(side, limits));
        let new = change.new.map(|side| descriptor(side, limits));
        let kind_changed = matches!((&old, &new), (Some((a, _)), Some((b, _))) if a.kind != b.kind);
        let content_changed =
            old.as_ref().map(|(d, _)| d.content) != new.as_ref().map(|(d, _)| d.content);
        let change_kind = classify_change(old.is_some(), new.is_some(), kind_changed);
        let presentation = presentation(&old, &new, kind_changed, limits, &mut budget)?;
        let truncated = matches!(presentation, DiffPresentation::Omitted { .. });
        files.push(FileDiff {
            path: change.path,
            change: change_kind,
            old: old.map(|v| v.0),
            new: new.map(|v| v.0),
            content_changed,
            kind_changed,
            presentation,
            truncated,
        });
    }
    Ok(DiffDocument {
        schema_version: 1,
        comparison,
        files,
    })
}

fn descriptor(
    side: crate::repository_diff::TreeSideInput,
    limits: DiffLimits,
) -> (DiffDescriptor, Option<Vec<u8>>) {
    let bytes = side.bytes;
    let size = bytes.as_ref().map_or(side.min_bytes, Vec::len);
    let text = bytes.as_deref().map(|bytes| {
        side.value.kind != FileKind::Symlink && text_capture::classify(bytes) == CaptureKind::Text
    });
    let inline = bytes.filter(|_| text == Some(true) && size <= limits.max_inline_bytes_per_side);
    (
        DiffDescriptor {
            kind: side.value.kind,
            content: side.value.content,
            bytes: size,
            text,
        },
        inline,
    )
}

pub fn build(scan: WorkspaceScan, limits: DiffLimits, deadline: Instant) -> Result<DiffDocument> {
    let comparison = DiffComparison::WorkspaceVsPinned {
        materialized_view: scan.materialized_view,
        materialized_head_generation: scan.materialized_head_generation,
        current_head_generation: scan.current_head_generation,
        authority_advanced: scan.authority_advanced,
    };
    let mut budget = Budget::new(limits, deadline);
    let mut files = Vec::with_capacity(scan.changes.len());
    for change in scan.changes {
        budget.charge_path(&change.path)?;
        let kind = classify_change(true, change.workspace.is_some(), change.kind_changed);
        let presentation = if change.workspace.is_none() {
            if let Some(old) = change.baseline_text.as_deref() {
                text_presentation(old, &[], limits, &mut budget)?
            } else {
                opaque(&change.baseline)
            }
        } else if change.kind_changed {
            let diff = match (
                change.baseline_text.as_deref(),
                change.workspace_text.as_deref(),
            ) {
                (Some(old), Some(new))
                    if change.baseline.kind != FileKind::Symlink
                        && change
                            .workspace
                            .as_ref()
                            .is_some_and(|side| side.kind != FileKind::Symlink) =>
                {
                    text_diff(old, new, limits, &mut budget)?
                }
                _ => None,
            };
            DiffPresentation::Kind { diff }
        } else if change.baseline.kind == FileKind::Symlink {
            DiffPresentation::Symlink
        } else if change.baseline.text == Some(false)
            || change
                .workspace
                .as_ref()
                .is_some_and(|side| side.text == Some(false))
        {
            DiffPresentation::Binary
        } else if let (Some(old), Some(new)) = (
            change.baseline_text.as_deref(),
            change.workspace_text.as_deref(),
        ) {
            text_presentation(old, new, limits, &mut budget)?
        } else {
            DiffPresentation::Omitted {
                reason: DiffOmissionReason::InlineBytes,
            }
        };
        let truncated = matches!(presentation, DiffPresentation::Omitted { .. });
        files.push(FileDiff {
            path: change.path,
            change: kind,
            old: Some(change.baseline),
            new: change.workspace,
            content_changed: change.content_changed,
            kind_changed: change.kind_changed,
            presentation,
            truncated,
        });
    }
    Ok(DiffDocument {
        schema_version: 1,
        comparison,
        files,
    })
}

fn classify_change(old: bool, new: bool, kind_changed: bool) -> FileChangeKind {
    match (old, new, kind_changed) {
        (false, true, _) => FileChangeKind::Added,
        (true, false, _) => FileChangeKind::Deleted,
        (true, true, true) => FileChangeKind::KindChanged,
        _ => FileChangeKind::Modified,
    }
}

fn opaque(descriptor: &DiffDescriptor) -> DiffPresentation {
    if descriptor.kind == FileKind::Symlink {
        DiffPresentation::Symlink
    } else if descriptor.text == Some(false) {
        DiffPresentation::Binary
    } else {
        DiffPresentation::Omitted {
            reason: DiffOmissionReason::InlineBytes,
        }
    }
}

fn presentation(
    old: &Option<(DiffDescriptor, Option<Vec<u8>>)>,
    new: &Option<(DiffDescriptor, Option<Vec<u8>>)>,
    kind_changed: bool,
    limits: DiffLimits,
    budget: &mut Budget,
) -> Result<DiffPresentation> {
    if kind_changed {
        let diff = match (old, new) {
            (Some((a, Some(old))), Some((b, Some(new))))
                if a.kind != FileKind::Symlink && b.kind != FileKind::Symlink =>
            {
                text_diff(old, new, limits, budget)?
            }
            _ => None,
        };
        return Ok(DiffPresentation::Kind { diff });
    }
    match (old, new) {
        (None, Some((d, Some(text)))) if d.kind != FileKind::Symlink => {
            text_presentation(&[], text, limits, budget)
        }
        (Some((d, Some(text))), None) if d.kind != FileKind::Symlink => {
            text_presentation(text, &[], limits, budget)
        }
        (Some((a, Some(old))), Some((b, Some(new))))
            if a.kind != FileKind::Symlink && b.kind != FileKind::Symlink =>
        {
            text_presentation(old, new, limits, budget)
        }
        (Some((d, _)), None) | (None, Some((d, _))) => Ok(opaque(d)),
        (Some((d, _)), Some(_)) if d.kind == FileKind::Symlink => Ok(DiffPresentation::Symlink),
        _ => Ok(DiffPresentation::Binary),
    }
}

fn text_diff(
    old: &[u8],
    new: &[u8],
    limits: DiffLimits,
    budget: &mut Budget,
) -> Result<Option<DiffText>> {
    match text_presentation(old, new, limits, budget)? {
        DiffPresentation::Text { diff } => Ok(Some(diff)),
        DiffPresentation::Omitted { .. } => Ok(None),
        _ => unreachable!("text builder returns text or omitted"),
    }
}

fn text_presentation(
    old: &[u8],
    new: &[u8],
    limits: DiffLimits,
    budget: &mut Budget,
) -> Result<DiffPresentation> {
    let work = old.len().saturating_add(new.len());
    if budget.algorithms_exhausted || Instant::now() >= budget.deadline {
        budget.algorithms_exhausted = true;
        return Ok(DiffPresentation::Omitted {
            reason: DiffOmissionReason::Deadline,
        });
    }
    if work > budget.work {
        budget.work = 0;
        budget.algorithms_exhausted = true;
        return Ok(DiffPresentation::Omitted {
            reason: DiffOmissionReason::WorkBudget,
        });
    }
    budget.work -= work;
    let old_lines = line_count_up_to(old, limits.max_lines_per_side);
    let new_lines = line_count_up_to(new, limits.max_lines_per_side);
    if old_lines.is_none() || new_lines.is_none() {
        return Ok(DiffPresentation::Omitted {
            reason: DiffOmissionReason::LineCount,
        });
    }
    let line_work = old_lines
        .unwrap_or(0)
        .saturating_add(new_lines.unwrap_or(0));
    if line_work > budget.work {
        budget.work = 0;
        budget.algorithms_exhausted = true;
        return Ok(DiffPresentation::Omitted {
            reason: DiffOmissionReason::WorkBudget,
        });
    }
    budget.work -= line_work;
    let old = std::str::from_utf8(old).expect("canonical text classification");
    let new = std::str::from_utf8(new).expect("canonical text classification");
    let mut config = TextDiff::configure();
    config.deadline(budget.deadline);
    let diff = config.diff_lines(old, new);
    if Instant::now() >= budget.deadline {
        budget.algorithms_exhausted = true;
        return Ok(DiffPresentation::Omitted {
            reason: DiffOmissionReason::Deadline,
        });
    }
    let groups = diff.grouped_ops(limits.context_lines);
    let mut text = String::new();
    let mut hunks = Vec::with_capacity(groups.len());
    for group in groups {
        let first = group.first().expect("diff group is nonempty");
        let last = group.last().expect("diff group is nonempty");
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let mut lines = Vec::new();
        for operation in group {
            for change in diff.iter_changes(&operation) {
                if budget.lines == 0 {
                    return Ok(DiffPresentation::Omitted {
                        reason: DiffOmissionReason::ModelBytes,
                    });
                }
                budget.lines -= 1;
                let start = text.len();
                let value_len = change.value().len();
                if value_len > budget.text {
                    budget.text = 0;
                    return Ok(DiffPresentation::Omitted {
                        reason: DiffOmissionReason::ModelBytes,
                    });
                }
                budget.text -= value_len;
                let end = start + value_len;
                text.push_str(change.value());
                lines.push(DiffLine {
                    operation: match change.tag() {
                        ChangeTag::Equal => DiffLineOperation::Context,
                        ChangeTag::Delete => DiffLineOperation::Delete,
                        ChangeTag::Insert => DiffLineOperation::Insert,
                    },
                    old_line: change.old_index().map(|i| i + 1),
                    new_line: change.new_index().map(|i| i + 1),
                    text_start: start,
                    text_end: end,
                });
            }
        }
        hunks.push(DiffHunk {
            old_start: hunk_start(old_range.start, old_range.len()),
            old_count: old_range.len(),
            new_start: hunk_start(new_range.start, new_range.len()),
            new_count: new_range.len(),
            lines,
        });
    }
    Ok(DiffPresentation::Text {
        diff: DiffText { text, hunks },
    })
}

fn hunk_start(zero_based: usize, count: usize) -> usize {
    if count == 0 {
        zero_based
    } else {
        zero_based + 1
    }
}

fn line_count_up_to(bytes: &[u8], limit: usize) -> Option<usize> {
    let newlines = bytes
        .iter()
        .filter(|b| **b == b'\n')
        .take(limit.saturating_add(1))
        .count();
    let lines = newlines.saturating_add(usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n")));
    (lines <= limit).then_some(lines)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wheat_core::diff_model::{DiffLimits, DiffOmissionReason, DiffPresentation};

    use super::{Budget, hunk_start, line_count_up_to, text_presentation};

    #[test]
    fn line_boundaries() {
        assert_eq!(line_count_up_to(b"one\ntwo", 2), Some(2));
        assert_eq!(line_count_up_to(b"one\ntwo", 1), None);
        assert_eq!(line_count_up_to(b"one\n", 1), Some(1));
    }

    #[test]
    fn empty_hunk_side_uses_preceding_position() {
        assert_eq!(hunk_start(0, 0), 0);
        assert_eq!(hunk_start(2, 0), 2);
        assert_eq!(hunk_start(2, 1), 3);
    }

    #[test]
    fn separates_distant_changes_and_computes_ranges() {
        let old = b"a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let new = b"A\nb\nc\nd\ne\nf\ng\nh\nI\n";
        let limits = DiffLimits {
            context_lines: 1,
            ..DiffLimits::default()
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut budget = Budget::new(limits, deadline);
        let DiffPresentation::Text { diff } =
            text_presentation(old, new, limits, &mut budget).unwrap()
        else {
            panic!("expected text diff")
        };
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!((diff.hunks[0].old_start, diff.hunks[0].old_count), (1, 2));
        assert_eq!((diff.hunks[1].new_start, diff.hunks[1].new_count), (8, 2));
    }

    #[test]
    fn exact_budgets_pass_and_one_less_omits() {
        let bytes = b"old\n";
        let mut limits = DiffLimits {
            max_work_units: bytes.len() + 1,
            ..DiffLimits::default()
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut budget = Budget::new(limits, deadline);
        assert!(matches!(
            text_presentation(bytes, b"", limits, &mut budget).unwrap(),
            DiffPresentation::Text { .. }
        ));
        assert_eq!(budget.work, 0);

        limits.max_work_units -= 1;
        let mut budget = Budget::new(limits, deadline);
        assert!(matches!(
            text_presentation(bytes, b"", limits, &mut budget).unwrap(),
            DiffPresentation::Omitted {
                reason: DiffOmissionReason::WorkBudget
            }
        ));
    }

    #[test]
    fn exhausted_deadline_stops_all_later_text_algorithms() {
        let limits = DiffLimits::default();
        let mut budget = Budget::new(limits, Instant::now() - Duration::from_millis(1));
        for _ in 0..10_000 {
            assert!(matches!(
                text_presentation(b"a", b"b", limits, &mut budget).unwrap(),
                DiffPresentation::Omitted {
                    reason: DiffOmissionReason::Deadline
                }
            ));
        }
        assert!(budget.algorithms_exhausted);
    }
}
