//! Bounded, storage-neutral file-diff model.

use serde::{Deserialize, Serialize};

use crate::{ContentId, FileKind, RevisionId, TreeId, ViewId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffDocument {
    pub schema_version: u32,
    pub comparison: DiffComparison,
    pub files: Vec<FileDiff>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiffComparison {
    WorkspaceVsPinned {
        materialized_view: ViewId,
        materialized_head_generation: u64,
        current_head_generation: u64,
        authority_advanced: bool,
    },
    Revision {
        revision: RevisionId,
        from_tree: TreeId,
        to_tree: TreeId,
        head_generation: u64,
    },
    Views {
        from_view: ViewId,
        to_view: ViewId,
        from_tree: TreeId,
        to_tree: TreeId,
        head_generation: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffDescriptor {
    pub kind: FileKind,
    pub content: ContentId,
    /// Exact for inspected content; for an exceeded bounded read this is a known lower bound.
    pub bytes: usize,
    /// `None` means the content exceeded the inspection bound, so text/binary classification is
    /// intentionally unknown rather than guessed.
    pub text: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub operation: DiffLineOperation,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text_start: usize,
    pub text_end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

/// Flat text storage avoids one heap allocation per rendered line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffText {
    pub text: String,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineOperation {
    Context,
    Delete,
    Insert,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiffPresentation {
    Text {
        diff: DiffText,
    },
    Binary,
    Symlink,
    /// Kind metadata changed; content hunks are included when both sides are textual files.
    Kind {
        diff: Option<DiffText>,
    },
    Omitted {
        reason: DiffOmissionReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOmissionReason {
    InlineBytes,
    LineCount,
    ModelBytes,
    WorkBudget,
    Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Deleted,
    Modified,
    KindChanged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub change: FileChangeKind,
    pub old: Option<DiffDescriptor>,
    pub new: Option<DiffDescriptor>,
    pub content_changed: bool,
    pub kind_changed: bool,
    pub presentation: DiffPresentation,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffLimits {
    pub max_inline_bytes_per_side: usize,
    pub max_lines_per_side: usize,
    pub max_model_text_bytes: usize,
    pub max_model_paths_bytes: usize,
    pub max_line_entries: usize,
    pub max_work_units: usize,
    pub context_lines: usize,
    pub deadline: std::time::Duration,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_inline_bytes_per_side: 1024 * 1024,
            max_lines_per_side: 100_000,
            max_model_text_bytes: 4 * 1024 * 1024,
            max_model_paths_bytes: 4 * 1024 * 1024,
            max_line_entries: 250_000,
            max_work_units: 8 * 1024 * 1024,
            context_lines: 3,
            deadline: std::time::Duration::from_millis(250),
        }
    }
}
