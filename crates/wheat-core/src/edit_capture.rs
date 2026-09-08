//! Pure conversion of authenticated before/after bytes into canonical edit records.
//!
//! This adapter is deliberately unaware of Git and the filesystem. Callers remain
//! responsible for authenticating the bytes they supply.

use std::{borrow::Cow, collections::BTreeMap, ops::Range};

use thiserror::Error;

use crate::{
    CanonicalObjectError, ContentId, EditId, EditOperation, EditRecord, FileKind, FileValue,
    guarded::{self, Limits},
    text_capture::{self, CapturedBytes},
};

/// One side of a captured file transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileSnapshot<'a> {
    pub kind: FileKind,
    pub bytes: &'a [u8],
}

/// Identifies the snapshot which exceeded the configured capture bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotSide {
    Before,
    After,
}

/// A canonical transition and all objects needed to persist its edits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedEdit {
    pub before: Option<FileValue>,
    pub after: Option<FileValue>,
    pub edits: BTreeMap<EditId, EditRecord>,
    /// Canonical file bodies and guarded-edit fragments, keyed by their digest.
    pub content: BTreeMap<ContentId, Vec<u8>>,
}

/// Failure to validate or derive a captured transition.
#[derive(Debug, Error)]
pub enum EditCaptureError {
    #[error("invalid edit path: {0}")]
    InvalidPath(#[source] CanonicalObjectError),
    #[error("{side:?} snapshot is {actual} bytes, exceeding limit {limit}")]
    InputTooLarge {
        side: SnapshotSide,
        actual: usize,
        limit: usize,
    },
    #[error("explicit guard range {index} is invalid")]
    InvalidGuardRange { index: usize },
    #[error("explicit guard ranges are unsupported for this file transition")]
    UnsupportedExplicitGuards,
    #[error("capture would produce {actual} edits, exceeding limit {limit}")]
    TooManyEdits { actual: usize, limit: usize },
    #[error("guarded edit authoring failed: {0:?}")]
    Authoring(guarded::authoring::AuthoringError),
    #[error("derived edit record is invalid: {0}")]
    InvalidRecord(#[source] CanonicalObjectError),
}

/// Limits for capturing a file transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    /// Maximum bytes accepted for either complete file snapshot.
    pub max_file_bytes: usize,
    /// Maximum lines on either side of a guarded text transition. Text above
    /// this bound remains importable as an exact whole-file replacement.
    pub max_guarded_lines: usize,
    /// Tighter work and size limits for deriving guarded text edits.
    pub guarded: Limits,
}

impl From<Limits> for CaptureLimits {
    fn from(guarded: Limits) -> Self {
        Self {
            max_file_bytes: guarded.max_input_bytes,
            max_guarded_lines: crate::guarded::line_diff::DEFAULT_MAX_LINES,
            guarded,
        }
    }
}

/// Convert borrowed, authenticated snapshots into canonical edit definitions.
///
/// Regular and executable UTF-8 files without NUL bytes are canonicalized to LF
/// before identity derivation. Symlinks and binary files remain byte exact.
/// Explicit guard ranges use byte offsets into the canonical LF `after` workspace,
/// not the caller's original line-ending representation. Nonempty ranges are only
/// supported for changed text-to-text transitions of the same file kind.
/// Existing text-to-text transitions within `limits.max_input_bytes` are always sent
/// through guarded authoring; an authoring failure is returned and never converted
/// into an `ExactFile` edit.
pub fn capture_edit(
    path: &str,
    before: Option<FileSnapshot<'_>>,
    after: Option<FileSnapshot<'_>>,
    explicit_guard_ranges: Option<&[Range<usize>]>,
    limits: Limits,
) -> Result<CapturedEdit, EditCaptureError> {
    capture_edit_with_limits(path, before, after, explicit_guard_ranges, limits.into())
}

/// Capture with a whole-file bound independent from the guarded-text bound.
///
/// A text transition with either side above the guarded bound is represented as one
/// exact whole-file replacement. This keeps matching and diff work bounded while
/// allowing repositories to retain large files as ordinary content-addressed values.
pub fn capture_edit_with_limits(
    path: &str,
    before: Option<FileSnapshot<'_>>,
    after: Option<FileSnapshot<'_>>,
    explicit_guard_ranges: Option<&[Range<usize>]>,
    limits: CaptureLimits,
) -> Result<CapturedEdit, EditCaptureError> {
    // Validate cheap borrowed metadata before line-ending normalization or copies.
    validate_path(path).map_err(EditCaptureError::InvalidPath)?;
    check_bound(before, SnapshotSide::Before, limits.max_file_bytes)?;
    check_bound(after, SnapshotSide::After, limits.max_file_bytes)?;

    let before = before.map(canonicalize);
    let after = after.map(canonicalize);

    let before_value = before.as_ref().map(CanonicalSnapshot::value);
    let after_value = after.as_ref().map(CanonicalSnapshot::value);
    let changed = before_value != after_value;
    let guarded_text = changed
        && matches!((&before, &after), (Some(a), Some(b))
            if a.kind == b.kind
                && a.kind != FileKind::Symlink
                && guarded_text_eligible(a, limits.guarded, limits.max_guarded_lines)
                && guarded_text_eligible(b, limits.guarded, limits.max_guarded_lines));
    if explicit_guard_ranges.is_some_and(|ranges| !ranges.is_empty()) && !guarded_text {
        return Err(EditCaptureError::UnsupportedExplicitGuards);
    }
    if let Some(ranges) = explicit_guard_ranges {
        let workspace = after
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.bytes.as_ref());
        for (index, range) in ranges.iter().enumerate() {
            if range.start > range.end
                || range.end > workspace.len()
                || !is_utf8_boundary(workspace, range.start)
                || !is_utf8_boundary(workspace, range.end)
            {
                return Err(EditCaptureError::InvalidGuardRange { index });
            }
        }
    }

    let mut result = CapturedEdit {
        before: before_value,
        after: after_value,
        edits: BTreeMap::new(),
        content: BTreeMap::new(),
    };

    if !changed {
        insert_snapshots(&mut result.content, before, after);
        return Ok(result);
    }

    if guarded_text {
        let before = before.as_ref().expect("matched above");
        let after = after.as_ref().expect("matched above");
        let definitions = guarded::authoring::normalize(
            &before.bytes,
            &[],
            &before.bytes,
            &after.bytes,
            explicit_guard_ranges,
            limits.guarded,
        )
        .map_err(EditCaptureError::Authoring)?;
        for definition in definitions {
            let before_id = insert_content(&mut result.content, &definition.before);
            let expected_id = insert_content(&mut result.content, &definition.expected);
            let replacement_id = insert_content(&mut result.content, &definition.replacement);
            let after_id = insert_content(&mut result.content, &definition.after);
            let record = EditRecord::new(
                path,
                EditOperation::GuardedText {
                    kind: before.kind,
                    before: before_id,
                    expected: expected_id,
                    replacement: replacement_id,
                    after: after_id,
                    start_anchor: definition.start_anchor,
                    end_anchor: definition.end_anchor,
                },
            )
            .map_err(EditCaptureError::InvalidRecord)?;
            result.edits.insert(record.id(), record);
        }
    } else {
        if limits.guarded.max_edits == 0 {
            return Err(EditCaptureError::TooManyEdits {
                actual: 1,
                limit: limits.guarded.max_edits,
            });
        }
        let record = EditRecord::new(
            path,
            EditOperation::ExactFile {
                expected: before_value,
                replacement: after_value,
            },
        )
        .map_err(EditCaptureError::InvalidRecord)?;
        result.edits.insert(record.id(), record);
    }
    insert_snapshots(&mut result.content, before, after);
    Ok(result)
}

#[derive(Debug)]
struct CanonicalSnapshot<'a> {
    kind: FileKind,
    bytes: Cow<'a, [u8]>,
    is_text: bool,
}

impl CanonicalSnapshot<'_> {
    fn value(&self) -> FileValue {
        FileValue {
            content: ContentId::of(&self.bytes),
            kind: self.kind,
        }
    }
}

fn canonicalize(snapshot: FileSnapshot<'_>) -> CanonicalSnapshot<'_> {
    if snapshot.kind == FileKind::Symlink {
        return CanonicalSnapshot {
            kind: snapshot.kind,
            bytes: Cow::Borrowed(snapshot.bytes),
            is_text: false,
        };
    }
    match text_capture::capture(snapshot.bytes) {
        CapturedBytes::Text(Cow::Borrowed(text)) => CanonicalSnapshot {
            kind: snapshot.kind,
            bytes: Cow::Borrowed(text.as_bytes()),
            is_text: true,
        },
        CapturedBytes::Text(Cow::Owned(text)) => CanonicalSnapshot {
            kind: snapshot.kind,
            bytes: Cow::Owned(text.into_bytes()),
            is_text: true,
        },
        CapturedBytes::Binary(bytes) => CanonicalSnapshot {
            kind: snapshot.kind,
            bytes,
            is_text: false,
        },
    }
}

fn guarded_text_eligible(
    snapshot: &CanonicalSnapshot<'_>,
    limits: Limits,
    max_lines: usize,
) -> bool {
    snapshot.is_text
        && snapshot.bytes.len() <= limits.max_input_bytes
        && line_count(&snapshot.bytes) <= max_lines
}

fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
}

fn check_bound(
    snapshot: Option<FileSnapshot<'_>>,
    side: SnapshotSide,
    limit: usize,
) -> Result<(), EditCaptureError> {
    if let Some(snapshot) = snapshot
        && snapshot.bytes.len() > limit
    {
        return Err(EditCaptureError::InputTooLarge {
            side,
            actual: snapshot.bytes.len(),
            limit,
        });
    }
    Ok(())
}

fn insert_content(content: &mut BTreeMap<ContentId, Vec<u8>>, bytes: &[u8]) -> ContentId {
    let id = ContentId::of(bytes);
    content.entry(id).or_insert_with(|| bytes.to_vec());
    id
}

fn insert_snapshots(
    content: &mut BTreeMap<ContentId, Vec<u8>>,
    before: Option<CanonicalSnapshot<'_>>,
    after: Option<CanonicalSnapshot<'_>>,
) {
    for snapshot in [before, after].into_iter().flatten() {
        let id = ContentId::of(&snapshot.bytes);
        content
            .entry(id)
            .or_insert_with(|| snapshot.bytes.into_owned());
    }
}

fn is_utf8_boundary(bytes: &[u8], offset: usize) -> bool {
    offset == bytes.len()
        || bytes
            .get(offset)
            .is_some_and(|byte| byte & 0b1100_0000 != 0b1000_0000)
}

fn validate_path(path: &str) -> Result<(), CanonicalObjectError> {
    if path.is_empty() {
        return crate::validate_directory_component(path);
    }
    for component in path.split('/') {
        crate::validate_directory_component(component)?;
    }
    Ok(())
}
