//! Placing a workspace edit into an earlier change.
//!
//! The workspace shows the result of every change. When the author adds an edit to change B
//! while later changes also touched the file, the edit was made against bytes some of which
//! only exist because of those later changes. This module works out who wrote each byte of the
//! current file ([`attribute`]) and then rebuilds B's version of the file ([`place`]):
//!
//! - an edit that touches only bytes B already had goes straight into B;
//! - an edit that overlaps a block a later change wrote takes that whole block into B, and the
//!   later change gives it up;
//! - when a later change created, deleted, or replaced the whole file, or the workspace changes
//!   the file's kind, the whole file moves into B.
//!
//! Everything here is pure. The caller supplies the later changes' applied write evidence and
//! checks the result by replaying the later changes' remaining edits.

use std::{collections::BTreeSet, ops::Range};

use crate::{
    ChangeId, EditId,
    guarded::line_diff::{LineDiffLimits, line_diff},
};

/// One edit of a change that comes after the change being revised.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LaterEdit {
    pub change: ChangeId,
    pub edit: EditId,
}

/// Who wrote a run of bytes in the current file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    /// Bytes of the earlier change's own result.
    Below,
    /// Bytes a later change's edit wrote. An empty run marks where it deleted bytes.
    Above(LaterEdit),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    /// The run's bytes in the current file.
    pub range: Range<usize>,
    /// The bytes of the earlier change's result the run stands for: its own bytes for
    /// [`Origin::Below`], the bytes the later edit replaced or deleted for [`Origin::Above`].
    /// The runs' `below` ranges follow each other and cover the earlier change's result.
    pub below: Range<usize>,
    pub origin: Origin,
}

/// One later change's applied edits on the path, in the order changes are composed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaterStep {
    pub change: ChangeId,
    pub writes: Vec<LaterWrite>,
}

/// Applied evidence of one edit, in the coordinates of the file it was applied to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaterWrite {
    /// A guarded edit replaced `write` with `replacement_len` bytes.
    Range {
        edit: EditId,
        write: Range<usize>,
        replacement_len: usize,
    },
    /// An exact edit set the whole file. `len` is `None` when it deleted the file.
    WholeFile { edit: EditId, len: Option<usize> },
}

/// Who wrote each byte of the current file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Provenance {
    /// Runs covering the current file in order, or `None` when the file is absent.
    pub segments: Option<Vec<Segment>>,
    /// Later edits that created, deleted, or replaced the whole file.
    pub whole_file: Vec<LaterEdit>,
    /// Every later edit that wrote the path, in application order.
    pub edits: Vec<LaterEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlacementError {
    #[error("applied edit evidence does not fit the file it was applied to")]
    InconsistentEvidence,
}

/// One run covering a file of `len` bytes (`None` when absent), standing for all `below_len`
/// bytes of the earlier change's result.
fn whole(len: Option<usize>, below_len: usize, origin: Origin) -> Option<Vec<Segment>> {
    len.map(|len| {
        let mut segments = Vec::new();
        if len > 0 || below_len > 0 {
            segments.push(Segment {
                range: 0..len,
                below: 0..below_len,
                origin,
            });
        }
        segments
    })
}

/// Follows `steps` from the earlier change's result (`below_len` bytes, `None` if absent) to
/// the current file, recording who wrote each byte.
#[tracing::instrument(level = "debug", skip_all, fields(steps = steps.len()))]
pub fn attribute(
    below_len: Option<usize>,
    steps: &[LaterStep],
) -> Result<Provenance, PlacementError> {
    let below_total = below_len.unwrap_or(0);
    let mut provenance = Provenance {
        segments: whole(below_len, below_total, Origin::Below),
        ..Provenance::default()
    };
    let mut ranges = Vec::new();
    for step in steps {
        ranges.clear();
        let mut whole_file = None;
        for write in &step.writes {
            let later = |edit: &EditId| LaterEdit {
                change: step.change,
                edit: *edit,
            };
            match write {
                LaterWrite::WholeFile { edit, len } => {
                    if whole_file.replace((later(edit), *len)).is_some() {
                        return Err(PlacementError::InconsistentEvidence);
                    }
                }
                LaterWrite::Range {
                    edit,
                    write,
                    replacement_len,
                } => ranges.push((later(edit), write.clone(), *replacement_len)),
            }
        }
        match whole_file {
            Some(_) if !ranges.is_empty() => return Err(PlacementError::InconsistentEvidence),
            Some((later, len)) => {
                provenance.whole_file.push(later);
                provenance.edits.push(later);
                provenance.segments = whole(len, below_total, Origin::Above(later));
            }
            None if ranges.is_empty() => {}
            None => {
                ranges.sort_by_key(|(_, write, _)| (write.start, write.end));
                let segments = provenance
                    .segments
                    .as_deref()
                    .ok_or(PlacementError::InconsistentEvidence)?;
                provenance.segments = Some(rewrite(segments, below_total, &ranges)?);
                provenance
                    .edits
                    .extend(ranges.iter().map(|(later, _, _)| *later));
            }
        }
    }
    Ok(provenance)
}

/// The runs of the file after `writes` (sorted, disjoint) replaced parts of `segments`.
/// Deletion markers strictly inside a write are dropped; markers on a write's edge stay.
///
/// A write stands for the earlier bytes of everything it replaced, markers it dropped
/// included. Where it cuts a later block, the block's piece before the cut keeps the block's
/// earlier bytes.
fn rewrite(
    segments: &[Segment],
    below_len: usize,
    writes: &[(LaterEdit, Range<usize>, usize)],
) -> Result<Vec<Segment>, PlacementError> {
    let len = segments.last().map_or(0, |segment| segment.range.end);
    let mut out = Vec::with_capacity(segments.len() + 2 * writes.len());
    let mut copier = Copier {
        segments,
        next: 0,
        markers_from: 0,
    };
    let mut cursor = BelowCursor {
        segments,
        below_len,
        next: 0,
        before: 0,
    };
    let mut copied_to = 0;
    let mut shift = 0isize;
    for (later, write, replacement_len) in writes {
        if write.start < copied_to || write.end < write.start || write.end > len {
            return Err(PlacementError::InconsistentEvidence);
        }
        copier.copy(copied_to..write.start, shift, &mut out);
        let start = shifted(write.start, shift);
        // Markers on the write's start are copied before it; for an insertion, on its end too.
        let below_start = cursor.after_markers(write.start);
        let below_end = if write.is_empty() {
            below_start
        } else {
            cursor.before_markers(write.end)
        };
        out.push(Segment {
            range: start..start + replacement_len,
            below: below_start..below_end,
            origin: Origin::Above(*later),
        });
        shift += *replacement_len as isize - (write.end - write.start) as isize;
        copied_to = write.end;
    }
    copier.copy(copied_to..len, shift, &mut out);
    Ok(out)
}

fn shifted(position: usize, shift: isize) -> usize {
    position
        .checked_add_signed(shift)
        .expect("writes never shift a position before the start of the file")
}

/// Copies spans of increasing position out of `segments` in one pass.
struct Copier<'a> {
    segments: &'a [Segment],
    /// Segments before this index end before every later span.
    next: usize,
    /// Markers before this index were already copied.
    markers_from: usize,
}

impl Copier<'_> {
    /// Copies the parts of the segments inside `span`, moved by `shift`, including deletion
    /// markers on either edge of it.
    fn copy(&mut self, span: Range<usize>, shift: isize, out: &mut Vec<Segment>) {
        while let Some(segment) = self.segments.get(self.next) {
            let before = if segment.range.is_empty() {
                segment.range.start < span.start
            } else {
                segment.range.end <= span.start
            };
            if !before {
                break;
            }
            self.next += 1;
        }
        for (index, segment) in self.segments.iter().enumerate().skip(self.next) {
            if segment.range.start > span.end
                || (!segment.range.is_empty() && segment.range.start >= span.end)
            {
                break;
            }
            if segment.range.is_empty() {
                if index < self.markers_from {
                    continue;
                }
                self.markers_from = index + 1;
            }
            let start = segment.range.start.max(span.start);
            let end = segment.range.end.min(span.end);
            if start == end && !segment.range.is_empty() {
                // An empty span inside a run: nothing of the run lies in it.
                continue;
            }
            let below = match segment.origin {
                Origin::Below => {
                    let at =
                        |position: usize| segment.below.start + (position - segment.range.start);
                    at(start)..at(end)
                }
                // A later block's earlier bytes stay with its first piece.
                Origin::Above(_) if start == segment.range.start => segment.below.clone(),
                Origin::Above(_) => segment.below.end..segment.below.end,
            };
            out.push(Segment {
                range: shifted(start, shift)..shifted(end, shift),
                below,
                origin: segment.origin,
            });
        }
    }
}

/// Offsets in the earlier change's result matching positions of increasing value in the file
/// `segments` describe.
struct BelowCursor<'a> {
    segments: &'a [Segment],
    below_len: usize,
    /// The first non-empty run that does not end at or before the last position asked for.
    next: usize,
    /// The end of the earlier bytes of the non-empty runs before `next`.
    before: usize,
}

impl BelowCursor<'_> {
    /// Moves to the first non-empty run ending after `position`.
    fn seek(&mut self, position: usize) -> Option<&Segment> {
        while let Some(segment) = self.segments.get(self.next) {
            if !segment.range.is_empty() {
                if segment.range.end > position {
                    return Some(segment);
                }
                self.before = segment.below.end;
            }
            self.next += 1;
        }
        None
    }

    /// The offset inside the run holding `position` strictly inside it.
    fn inside(segment: &Segment, position: usize) -> usize {
        match segment.origin {
            Origin::Below => segment.below.start + (position - segment.range.start),
            // The earlier bytes stay with the piece before the cut.
            Origin::Above(_) => segment.below.end,
        }
    }

    /// The offset at `position`, after the earlier bytes of deletions there.
    fn after_markers(&mut self, position: usize) -> usize {
        let below_len = self.below_len;
        match self.seek(position) {
            None => below_len,
            Some(segment) if segment.range.start >= position => segment.below.start,
            Some(segment) => Self::inside(segment, position),
        }
    }

    /// The offset at `position`, before the earlier bytes of deletions there.
    fn before_markers(&mut self, position: usize) -> usize {
        match self.seek(position) {
            Some(segment) if segment.range.start < position => Self::inside(segment, position),
            _ => self.before,
        }
    }
}

/// The earlier change's file and the moves needed to hold the workspace's version of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    /// The earlier change's new version of the file, `None` to delete it.
    pub below: Option<Vec<u8>>,
    /// Later edits that move into the earlier change.
    pub moved: BTreeSet<LaterEdit>,
    pub notices: Vec<MoveNotice>,
}

/// Lines of the current file, 1-based and inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lines {
    pub first: usize,
    pub last: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WholeFileReason {
    /// The earlier change does not have the file; a later change created it.
    CreatedLater,
    /// A later change deleted the file.
    DeletedLater,
    /// A later change replaced the whole file.
    ReplacedLater,
    /// The workspace deletes the file or changes its kind.
    WorkspaceReplaces,
}

/// Why edits move, in terms of the current file the author sees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MoveNotice {
    /// The whole file moves; `changes` lose every edit they made to it.
    WholeFile {
        reason: WholeFileReason,
        changes: BTreeSet<ChangeId>,
    },
    /// The author's edit on `yours` overlaps `edit`'s block on `theirs`.
    Block {
        edit: LaterEdit,
        yours: Lines,
        theirs: Lines,
    },
    /// `edit` no longer fits after the author's edit, so it moves too.
    NoLongerFits { edit: LaterEdit, theirs: Lines },
}

/// The versions of one file the placement works from.
#[derive(Clone, Copy, Debug)]
pub struct PlacementInput<'a> {
    /// The earlier change's current result.
    pub below: Option<&'a [u8]>,
    /// The current file, after every later change.
    pub current: Option<&'a [u8]>,
    /// The workspace file in canonical form.
    pub workspace: Option<&'a [u8]>,
    /// The workspace changes the file's kind.
    pub kind_changed: bool,
    pub provenance: &'a Provenance,
    /// Later edits the caller found no longer fit; they move whatever they overlap.
    pub also_move: &'a BTreeSet<LaterEdit>,
}

/// Rebuilds the earlier change's version of the file so the composition ends in the workspace
/// file, moving later blocks the workspace edit overlaps.
#[tracing::instrument(level = "debug", skip_all)]
pub fn place(input: PlacementInput<'_>) -> Result<Placement, PlacementError> {
    let unchanged = !input.kind_changed && input.current == input.workspace;
    if input.provenance.edits.is_empty() || (unchanged && input.also_move.is_empty()) {
        let below = if input.provenance.edits.is_empty() {
            input.workspace
        } else {
            input.below
        };
        return Ok(Placement {
            below: below.map(<[u8]>::to_vec),
            moved: BTreeSet::new(),
            notices: Vec::new(),
        });
    }
    let whole_file = |reason| Ok(whole_file_placement(input, reason));
    let Some(below) = input.below else {
        return whole_file(WholeFileReason::CreatedLater);
    };
    let (Some(current), Some(segments)) = (input.current, input.provenance.segments.as_deref())
    else {
        return whole_file(WholeFileReason::DeletedLater);
    };
    if !input.provenance.whole_file.is_empty() {
        return whole_file(WholeFileReason::ReplacedLater);
    }
    let Some(workspace) = input.workspace.filter(|_| !input.kind_changed) else {
        return whole_file(WholeFileReason::WorkspaceReplaces);
    };
    validate(below.len(), current.len(), segments)?;
    let hunks = hunks(current, workspace);
    let lines = LineIndex::new(current);

    // The region is every hunk plus every block of a moved edit. It grows until no block of an
    // unmoved later edit overlaps it.
    let mut moved = BTreeSet::new();
    let mut notices = Vec::new();
    let mut region = hunks
        .iter()
        .map(|(current, _)| current.clone())
        .collect::<Vec<_>>();
    for edit in input.also_move {
        if moved.insert(*edit) {
            let blocks = blocks_of(segments, *edit);
            notices.push(MoveNotice::NoLongerFits {
                edit: *edit,
                theirs: lines.span(&blocks),
            });
            region.extend(blocks);
        }
    }
    loop {
        region = merge(region);
        let mut grew = false;
        for interval in region.clone() {
            for segment in segments {
                let Origin::Above(edit) = segment.origin else {
                    continue;
                };
                if moved.contains(&edit) || !overlaps(&interval, &segment.range) {
                    continue;
                }
                moved.insert(edit);
                let blocks = blocks_of(segments, edit);
                notices.push(MoveNotice::Block {
                    edit,
                    yours: lines.span(std::slice::from_ref(&interval)),
                    theirs: lines.span(&blocks),
                });
                region.extend(blocks);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    // The earlier change keeps its own bytes outside the region, and the bytes unmoved later
    // edits replace there, and takes the workspace's bytes inside it.
    let mut rebuilt = Vec::with_capacity(workspace.len());
    let mut below_cursor = 0;
    let mut first = 0;
    for interval in &region {
        let span = below_span(segments, &mut first, interval, &moved, below.len());
        let below_start = span.start.max(below_cursor);
        rebuilt.extend_from_slice(&below[below_cursor..below_start]);
        let workspace_range = workspace_position(&hunks, interval.start, Bias::Right)
            ..workspace_position(&hunks, interval.end, Bias::Left);
        rebuilt.extend_from_slice(&workspace[workspace_range]);
        below_cursor = span.end.max(below_start);
    }
    rebuilt.extend_from_slice(&below[below_cursor..]);
    Ok(Placement {
        below: Some(rebuilt),
        moved,
        notices,
    })
}

fn whole_file_placement(input: PlacementInput<'_>, reason: WholeFileReason) -> Placement {
    let moved = input
        .provenance
        .edits
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    Placement {
        below: input.workspace.map(<[u8]>::to_vec),
        notices: vec![MoveNotice::WholeFile {
            reason,
            changes: moved.iter().map(|edit| edit.change).collect(),
        }],
        moved,
    }
}

/// Checks that the runs cover both the current file and the earlier change's file in order,
/// and that the earlier change's own runs have the same bytes in both.
fn validate(
    below_len: usize,
    current_len: usize,
    segments: &[Segment],
) -> Result<(), PlacementError> {
    let (mut at, mut below_at) = (0, 0);
    for segment in segments {
        let follows = segment.range.start == at
            && segment.range.end >= at
            && segment.below.start == below_at
            && segment.below.end >= below_at;
        let own = segment.origin != Origin::Below || segment.range.len() == segment.below.len();
        if !follows || !own {
            return Err(PlacementError::InconsistentEvidence);
        }
        (at, below_at) = (segment.range.end, segment.below.end);
    }
    if at != current_len || below_at != below_len {
        return Err(PlacementError::InconsistentEvidence);
    }
    Ok(())
}

/// Changed regions between the current and workspace files as paired ranges. Identical leading
/// and trailing lines are skipped first, so a small edit in a large file diffs only its
/// neighbourhood. When the rest can't be line-diffed it is one region.
fn hunks(current: &[u8], workspace: &[u8]) -> Vec<(Range<usize>, Range<usize>)> {
    let shortest = current.len().min(workspace.len());
    let mut prefix = current
        .iter()
        .zip(workspace)
        .take_while(|(a, b)| a == b)
        .count();
    // Both files hold the same bytes before `prefix`, so a line start is one in both.
    while prefix > 0 && current[prefix - 1] != b'\n' {
        prefix -= 1;
    }
    let mut suffix = current
        .iter()
        .rev()
        .zip(workspace.iter().rev())
        .take(shortest - prefix)
        .take_while(|(a, b)| a == b)
        .count();
    // The shared tail must start at a line start in both files.
    while suffix > 0 {
        let current_start = current.len() - suffix;
        let workspace_start = workspace.len() - suffix;
        let line_start = |bytes: &[u8], at: usize| at == 0 || bytes[at - 1] == b'\n';
        if line_start(current, current_start) && line_start(workspace, workspace_start) {
            break;
        }
        suffix -= 1;
    }
    let current_middle = prefix..current.len() - suffix;
    let workspace_middle = prefix..workspace.len() - suffix;
    if current_middle.is_empty() && workspace_middle.is_empty() {
        return Vec::new();
    }
    match line_diff(
        &current[current_middle.clone()],
        &workspace[workspace_middle.clone()],
        LineDiffLimits::default(),
    ) {
        Ok(diff) => merge_hunks(
            diff.hunks
                .into_iter()
                .map(|hunk| {
                    let (current, workspace) = if hunk.ambiguous {
                        (hunk.before_envelope, hunk.after_envelope)
                    } else {
                        (hunk.before, hunk.after)
                    };
                    (
                        current.start + prefix..current.end + prefix,
                        workspace.start + prefix..workspace.end + prefix,
                    )
                })
                .collect(),
        ),
        // Not line-diffable text, or too large to diff line by line: one coarser region is
        // still exact, only less precise about what the author touched.
        Err(_) => vec![(current_middle, workspace_middle)],
    }
}

/// The ranges of the current file that `edit` wrote.
fn blocks_of(segments: &[Segment], edit: LaterEdit) -> Vec<Range<usize>> {
    segments
        .iter()
        .filter(|segment| segment.origin == Origin::Above(edit))
        .map(|segment| segment.range.clone())
        .collect()
}

/// Whether the author's `interval` touches a later block. An empty block (a deletion) or an
/// empty interval (an insertion) counts only strictly inside the other range.
fn overlaps(interval: &Range<usize>, block: &Range<usize>) -> bool {
    match (interval.is_empty(), block.is_empty()) {
        (false, false) => interval.start < block.end && block.start < interval.end,
        (false, true) => interval.start < block.start && block.start < interval.end,
        (true, false) => block.start < interval.start && interval.start < block.end,
        (true, true) => false,
    }
}

/// Sorts and merges ranges that overlap or touch.
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Merges hunks whose current or workspace ranges overlap, keeping both sides paired.
fn merge_hunks(mut hunks: Vec<(Range<usize>, Range<usize>)>) -> Vec<(Range<usize>, Range<usize>)> {
    hunks.sort_by_key(|(current, _)| (current.start, current.end));
    let mut merged: Vec<(Range<usize>, Range<usize>)> = Vec::with_capacity(hunks.len());
    for (current, workspace) in hunks {
        match merged.last_mut() {
            Some((last_current, last_workspace))
                if current.start < last_current.end || workspace.start < last_workspace.end =>
            {
                last_current.end = last_current.end.max(current.end);
                last_workspace.end = last_workspace.end.max(workspace.end);
            }
            _ => merged.push((current, workspace)),
        }
    }
    merged
}

#[derive(Clone, Copy)]
enum Bias {
    Left,
    Right,
}

/// The bytes of the earlier change's result that `interval` of the current file stands for:
/// those of the runs inside it, counting deletions strictly inside it and deletions by moved
/// edits on its edges. An insertion stands for none and sits before the deletions at its
/// position. Intervals come in increasing order; `first` is where the next one's search starts.
fn below_span(
    segments: &[Segment],
    first: &mut usize,
    interval: &Range<usize>,
    moved: &BTreeSet<LaterEdit>,
    below_len: usize,
) -> Range<usize> {
    while let Some(segment) = segments.get(*first) {
        let before = if segment.range.is_empty() {
            segment.range.start < interval.start
        } else {
            segment.range.end <= interval.start
        };
        if !before {
            break;
        }
        *first += 1;
    }
    let mut span: Option<Range<usize>> = None;
    let mut insertion = None;
    for segment in &segments[*first..] {
        let range = &segment.range;
        if range.start > interval.end {
            break;
        }
        let inside = if range.is_empty() {
            (interval.start < range.start && range.start < interval.end)
                || matches!(segment.origin, Origin::Above(edit) if moved.contains(&edit))
        } else {
            range.start < interval.end && interval.start < range.end
        };
        if !inside {
            insertion.get_or_insert(segment.below.start);
            continue;
        }
        let piece = match segment.origin {
            Origin::Below => {
                let at = |position: usize| segment.below.start + (position - range.start);
                at(interval.start.max(range.start))..at(interval.end.min(range.end))
            }
            Origin::Above(_) => segment.below.clone(),
        };
        span = Some(match span {
            Some(span) => span.start..piece.end,
            None => piece,
        });
    }
    span.unwrap_or_else(|| {
        let at = insertion.unwrap_or(below_len);
        at..at
    })
}

/// The offset in the workspace file matching `position` in the current file, which is never
/// strictly inside a hunk. An insertion exactly at `position` counts after it for `Right`
/// (an interval's start) and before it for `Left` (an interval's end).
fn workspace_position(
    hunks: &[(Range<usize>, Range<usize>)],
    position: usize,
    bias: Bias,
) -> usize {
    let mut shift = 0isize;
    for (current, workspace) in hunks {
        let before = current.end < position
            || (current.end == position && (!current.is_empty() || matches!(bias, Bias::Left)));
        if !before {
            break;
        }
        shift += workspace.len() as isize - current.len() as isize;
    }
    shifted(position, shift)
}

/// Byte offset to line number lookup for one file.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(bytes: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        Self { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }

    /// The lines `ranges` cover; an empty range is the line it sits on.
    fn span(&self, ranges: &[Range<usize>]) -> Lines {
        let first = ranges.iter().map(|range| range.start).min().unwrap_or(0);
        let last = ranges
            .iter()
            .map(|range| range.end.saturating_sub(1).max(range.start))
            .max()
            .unwrap_or(first);
        Lines {
            first: self.line(first),
            last: self.line(last),
        }
    }
}

#[cfg(test)]
mod tests;
