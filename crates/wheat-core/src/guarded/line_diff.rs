//! A deterministic, bounded, line-oriented diff used to map before-text
//! coordinates to working-text coordinates.
//!
//! Lines include their terminating LF byte. Consequently `"x\n"` and `"x"`
//! contain different line values and a missing final newline is never hidden.
//! The implementation uses a flat LCS table; it does not allocate per cell.

use std::{collections::HashMap, ops::Range};

/// Default maximum lines accepted by guarded-edit authoring.
pub const DEFAULT_MAX_LINES: usize = 20_000;

/// Hard limits for [`line_diff`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineDiffLimits {
    /// Maximum length of either input, in bytes.
    pub max_input_bytes: usize,
    /// Maximum number of lines in either input.
    pub max_lines: usize,
    /// Maximum number of entries in the flat LCS table, including its border.
    pub max_cells: usize,
    /// Maximum abstract DP work units. A non-identical diff needs
    /// `2 * before_lines * after_lines + before_lines + after_lines` units.
    pub max_work: usize,
}

impl Default for LineDiffLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 4 * 1024 * 1024,
            max_lines: DEFAULT_MAX_LINES,
            max_cells: 4_000_000,
            max_work: 8_100_000,
        }
    }
}

/// A validation or resource-limit failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineDiffError {
    InputTooLarge {
        input: Input,
        actual: usize,
        limit: usize,
    },
    TooManyLines {
        input: Input,
        actual: usize,
        limit: usize,
    },
    NonUtf8 {
        input: Input,
    },
    NonCanonicalLineEnding {
        input: Input,
    },
    NulByte {
        input: Input,
    },
    CellBudgetExceeded {
        required: usize,
        limit: usize,
    },
    WorkBudgetExceeded {
        required: usize,
        limit: usize,
    },
    /// The bounded allocation failed after all numeric limits were checked.
    AllocationFailed,
    /// A limit calculation overflowed `usize`; no allocation was attempted.
    LimitOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Before,
    After,
}

/// A byte range known to be identical in both texts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EqualSpan {
    pub before: Range<usize>,
    pub after: Range<usize>,
}

/// A changed byte range between adjacent equal spans.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineHunk {
    /// The deterministic display hunk. Do not use its endpoints as provenance
    /// when [`Self::ambiguous`] is true.
    pub before: Range<usize>,
    pub after: Range<usize>,
    /// The smallest component bounded by matched anchors present in every
    /// optimal LCS. This contains the changed region for every optimal
    /// alignment, including equal repeated lines whose identity is uncertain.
    pub before_envelope: Range<usize>,
    pub after_envelope: Range<usize>,
    /// True when an adjacent matched boundary can differ in another optimal LCS.
    ///
    /// Ambiguity is local: an unrelated repeated line elsewhere does not taint
    /// this hunk. Callers must use the conservative envelopes rather than infer
    /// line identity from an ambiguous display boundary.
    pub ambiguous: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineDiff {
    pub equal: Vec<EqualSpan>,
    pub hunks: Vec<LineHunk>,
}

#[derive(Clone, Copy)]
struct Line {
    start: usize,
    end: usize,
    fingerprint: u64,
}

/// Extremal coordinates of every match which can occupy one LCS rank.
/// Storing extrema rather than candidates keeps this O(lines) alongside the
/// already-required flat DP table.
#[derive(Clone, Copy)]
struct RankPossible {
    count: usize,
    before_min: usize,
    before_max: usize,
    after_min: usize,
    after_max: usize,
}

/// Diff canonical LF UTF-8 `before` against canonical LF UTF-8 `after`.
///
/// The canonical alignment first takes an optimal equal-line diagonal. When no
/// such diagonal is available, an optimal before-line deletion wins a tie over
/// an after-line insertion. This stable rule makes output deterministic. The
/// `ambiguous` bit prevents this rule from being mistaken for evidence that a
/// repeated line has a unique identity.
pub fn line_diff(
    before: &[u8],
    after: &[u8],
    limits: LineDiffLimits,
) -> Result<LineDiff, LineDiffError> {
    let before_count = validate_and_count(before, Input::Before, limits)?;
    let after_count = validate_and_count(after, Input::After, limits)?;

    // Equality is the only alignment that is intrinsically unambiguous even
    // when the input contains repeated lines.
    if before == after {
        return Ok(LineDiff {
            equal: if before.is_empty() {
                Vec::new()
            } else {
                vec![EqualSpan {
                    before: 0..before.len(),
                    after: 0..after.len(),
                }]
            },
            hunks: Vec::new(),
        });
    }

    let before_lines = split_lines(before, before_count)?;
    let after_lines = split_lines(after, after_count)?;
    let before_occurrences = fingerprint_occurrences(&before_lines);
    let after_occurrences = fingerprint_occurrences(&after_lines);
    let mut common_prefix = 0;
    let mut prefix = 0;
    while common_prefix < before_count
        && common_prefix < after_count
        && lines_equal(
            before,
            before_lines[common_prefix],
            after,
            after_lines[common_prefix],
        )
    {
        if fingerprint_is_unique(
            before_lines[common_prefix].fingerprint,
            &before_occurrences,
            &after_occurrences,
        ) {
            prefix = common_prefix + 1;
        }
        common_prefix += 1;
    }
    let mut common_suffix = 0;
    let mut suffix_count = 0;
    while common_suffix < before_count - prefix
        && common_suffix < after_count - prefix
        && lines_equal(
            before,
            before_lines[before_count - 1 - common_suffix],
            after,
            after_lines[after_count - 1 - common_suffix],
        )
    {
        let before_index = before_count - 1 - common_suffix;
        if fingerprint_is_unique(
            before_lines[before_index].fingerprint,
            &before_occurrences,
            &after_occurrences,
        ) {
            suffix_count = common_suffix + 1;
        }
        common_suffix += 1;
    }
    let before_middle = &before_lines[prefix..before_count - suffix_count];
    let after_middle = &after_lines[prefix..after_count - suffix_count];

    // Enforce DP bounds against only the unmatched middle. Equal boundaries are
    // proven by fingerprint plus bytes and never participate in the matrix.
    let rows = before_middle
        .len()
        .checked_add(1)
        .ok_or(LineDiffError::LimitOverflow)?;
    let cols = after_middle
        .len()
        .checked_add(1)
        .ok_or(LineDiffError::LimitOverflow)?;
    let cells = rows.checked_mul(cols).ok_or(LineDiffError::LimitOverflow)?;
    if cells > limits.max_cells {
        if let Some(diff) = anchored_line_diff(before, after, &before_lines, &after_lines, limits)?
        {
            return Ok(diff);
        }
        return Err(LineDiffError::CellBudgetExceeded {
            required: cells,
            limit: limits.max_cells,
        });
    }
    let pairs = before_middle
        .len()
        .checked_mul(after_middle.len())
        .ok_or(LineDiffError::LimitOverflow)?;
    let work = pairs
        .checked_mul(2)
        .and_then(|v| v.checked_add(before_middle.len()))
        .and_then(|v| v.checked_add(after_middle.len()))
        .ok_or(LineDiffError::LimitOverflow)?;
    if work > limits.max_work {
        return Err(LineDiffError::WorkBudgetExceeded {
            required: work,
            limit: limits.max_work,
        });
    }

    let mut suffix = Vec::new();
    suffix
        .try_reserve_exact(cells)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    suffix.resize(cells, 0usize);
    for i in (0..before_middle.len()).rev() {
        for j in (0..after_middle.len()).rev() {
            let mut best = suffix[(i + 1) * cols + j].max(suffix[i * cols + j + 1]);
            if lines_equal(before, before_middle[i], after, after_middle[j]) {
                best = best.max(1 + suffix[(i + 1) * cols + j + 1]);
            }
            suffix[i * cols + j] = best;
        }
    }

    let lcs_len = suffix[0];
    // A candidate at a given LCS rank can occur in an optimal alignment. A
    // chosen anchor is mandatory exactly when it is the sole candidate at its
    // rank: every optimal LCS has one match at every rank, and every candidate
    // below is in an optimal LCS. Keeping these counts per rank lets unrelated
    // repeated lines remain local to their changed boundary.
    let possible_by_rank = possible_matches_by_rank(
        before,
        after,
        before_middle,
        after_middle,
        &suffix,
        cols,
        lcs_len,
    )?;

    let mut matches = Vec::new();
    matches
        .try_reserve_exact(lcs_len)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    let (mut i, mut j) = (0, 0);
    while i < before_middle.len() && j < after_middle.len() {
        let here = suffix[i * cols + j];
        let equal = lines_equal(before, before_middle[i], after, after_middle[j]);
        if equal && here == 1 + suffix[(i + 1) * cols + j + 1] {
            matches.push((prefix + i, prefix + j));
            i += 1;
            j += 1;
        } else if suffix[(i + 1) * cols + j] == here {
            i += 1; // documented stable tie: deletion before insertion
        } else {
            j += 1;
        }
    }

    let mut all_matches = Vec::with_capacity(prefix + matches.len() + suffix_count);
    all_matches.extend((0..prefix).map(|index| (index, index)));
    all_matches.extend(matches);
    all_matches.extend((0..suffix_count).map(|offset| {
        (
            before_count - suffix_count + offset,
            after_count - suffix_count + offset,
        )
    }));
    let mut all_possible = Vec::with_capacity(all_matches.len());
    all_possible.extend((0..prefix).map(|index| unique_possible(index, index)));
    all_possible.extend(possible_by_rank.into_iter().map(|possible| RankPossible {
        before_min: possible.before_min + prefix,
        before_max: possible.before_max + prefix,
        after_min: possible.after_min + prefix,
        after_max: possible.after_max + prefix,
        ..possible
    }));
    all_possible.extend((0..suffix_count).map(|offset| {
        unique_possible(
            before_count - suffix_count + offset,
            after_count - suffix_count + offset,
        )
    }));

    Ok(build_result(
        before.len(),
        after.len(),
        &before_lines,
        &after_lines,
        &all_matches,
        &all_possible,
    ))
}

fn validate_and_count(
    bytes: &[u8],
    input: Input,
    limits: LineDiffLimits,
) -> Result<usize, LineDiffError> {
    if bytes.len() > limits.max_input_bytes {
        return Err(LineDiffError::InputTooLarge {
            input,
            actual: bytes.len(),
            limit: limits.max_input_bytes,
        });
    }
    if bytes.contains(&0) {
        return Err(LineDiffError::NulByte { input });
    }
    std::str::from_utf8(bytes).map_err(|_| LineDiffError::NonUtf8 { input })?;
    if bytes.contains(&b'\r') {
        return Err(LineDiffError::NonCanonicalLineEnding { input });
    }
    let count = bytes.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
    if count > limits.max_lines {
        return Err(LineDiffError::TooManyLines {
            input,
            actual: count,
            limit: limits.max_lines,
        });
    }
    Ok(count)
}

fn anchored_line_diff(
    before: &[u8],
    after: &[u8],
    before_lines: &[Line],
    after_lines: &[Line],
    limits: LineDiffLimits,
) -> Result<Option<LineDiff>, LineDiffError> {
    let before_occurrences = exact_occurrences(before, before_lines);
    let after_occurrences = exact_occurrences(after, after_lines);
    let mut anchors = Vec::new();
    for (before_index, line) in before_lines.iter().enumerate() {
        let bytes = line_bytes(before, *line);
        let Some(&(before_count, _)) = before_occurrences.get(bytes) else {
            continue;
        };
        let Some(&(after_count, after_index)) = after_occurrences.get(bytes) else {
            continue;
        };
        if before_count == 1 && after_count == 1 {
            anchors.push((before_index, after_index));
        }
    }
    let increasing = longest_increasing_anchor_indices(&anchors);
    if increasing.is_empty() {
        return Ok(None);
    }

    let mut equal = Vec::new();
    let mut hunks = Vec::new();
    let mut before_start = 0;
    let mut after_start = 0;
    for anchor_index in increasing {
        let (before_anchor, after_anchor) = anchors[anchor_index];
        append_region_diff(
            before,
            after,
            before_lines,
            after_lines,
            before_start..before_anchor,
            after_start..after_anchor,
            limits,
            &mut equal,
            &mut hunks,
        )?;
        equal.push(EqualSpan {
            before: before_lines[before_anchor].start..before_lines[before_anchor].end,
            after: after_lines[after_anchor].start..after_lines[after_anchor].end,
        });
        before_start = before_anchor + 1;
        after_start = after_anchor + 1;
    }
    append_region_diff(
        before,
        after,
        before_lines,
        after_lines,
        before_start..before_lines.len(),
        after_start..after_lines.len(),
        limits,
        &mut equal,
        &mut hunks,
    )?;
    merge_adjacent_equal(&mut equal);
    Ok(Some(LineDiff { equal, hunks }))
}

#[allow(clippy::too_many_arguments)]
fn append_region_diff(
    before: &[u8],
    after: &[u8],
    before_lines: &[Line],
    after_lines: &[Line],
    before_range: Range<usize>,
    after_range: Range<usize>,
    limits: LineDiffLimits,
    equal: &mut Vec<EqualSpan>,
    hunks: &mut Vec<LineHunk>,
) -> Result<(), LineDiffError> {
    let before_bytes = line_range_bytes(before, before_lines, before_range.clone());
    let after_bytes = line_range_bytes(after, after_lines, after_range.clone());
    if before_bytes.is_empty() && after_bytes.is_empty() {
        return Ok(());
    }
    let local = line_diff(before_bytes, after_bytes, limits)?;
    let before_offset = byte_offset(before_lines, before_range.start, before.len());
    let after_offset = byte_offset(after_lines, after_range.start, after.len());
    equal.extend(local.equal.into_iter().map(|span| EqualSpan {
        before: shift_range(span.before, before_offset),
        after: shift_range(span.after, after_offset),
    }));
    hunks.extend(local.hunks.into_iter().map(|hunk| LineHunk {
        before: shift_range(hunk.before, before_offset),
        after: shift_range(hunk.after, after_offset),
        before_envelope: shift_range(hunk.before_envelope, before_offset),
        after_envelope: shift_range(hunk.after_envelope, after_offset),
        ambiguous: hunk.ambiguous,
    }));
    Ok(())
}

fn exact_occurrences<'a>(bytes: &'a [u8], lines: &[Line]) -> HashMap<&'a [u8], (usize, usize)> {
    let mut occurrences = HashMap::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let entry = occurrences
            .entry(line_bytes(bytes, *line))
            .or_insert((0, index));
        entry.0 += 1;
    }
    occurrences
}

fn longest_increasing_anchor_indices(anchors: &[(usize, usize)]) -> Vec<usize> {
    let mut tails = Vec::<usize>::new();
    let mut predecessors = vec![None; anchors.len()];
    for (index, &(_, after)) in anchors.iter().enumerate() {
        let position = tails.partition_point(|&tail| anchors[tail].1 < after);
        if position > 0 {
            predecessors[index] = Some(tails[position - 1]);
        }
        if position == tails.len() {
            tails.push(index);
        } else {
            tails[position] = index;
        }
    }
    let Some(mut current) = tails.last().copied() else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(tails.len());
    loop {
        result.push(current);
        let Some(previous) = predecessors[current] else {
            break;
        };
        current = previous;
    }
    result.reverse();
    result
}

fn line_range_bytes<'a>(bytes: &'a [u8], lines: &[Line], range: Range<usize>) -> &'a [u8] {
    let start = byte_offset(lines, range.start, bytes.len());
    let end = byte_offset(lines, range.end, bytes.len());
    &bytes[start..end]
}

fn byte_offset(lines: &[Line], index: usize, bytes_len: usize) -> usize {
    lines.get(index).map_or(bytes_len, |line| line.start)
}

fn shift_range(range: Range<usize>, offset: usize) -> Range<usize> {
    range.start + offset..range.end + offset
}

fn merge_adjacent_equal(equal: &mut Vec<EqualSpan>) {
    let mut write = 0;
    for read in 0..equal.len() {
        if write > 0
            && equal[write - 1].before.end == equal[read].before.start
            && equal[write - 1].after.end == equal[read].after.start
        {
            equal[write - 1].before.end = equal[read].before.end;
            equal[write - 1].after.end = equal[read].after.end;
        } else {
            if write != read {
                equal[write] = equal[read].clone();
            }
            write += 1;
        }
    }
    equal.truncate(write);
}

fn fingerprint_occurrences(lines: &[Line]) -> HashMap<u64, usize> {
    let mut occurrences = HashMap::with_capacity(lines.len());
    for line in lines {
        *occurrences.entry(line.fingerprint).or_insert(0) += 1;
    }
    occurrences
}

fn fingerprint_is_unique(
    fingerprint: u64,
    before: &HashMap<u64, usize>,
    after: &HashMap<u64, usize>,
) -> bool {
    before.get(&fingerprint) == Some(&1) && after.get(&fingerprint) == Some(&1)
}

fn unique_possible(before: usize, after: usize) -> RankPossible {
    RankPossible {
        count: 1,
        before_min: before,
        before_max: before,
        after_min: after,
        after_max: after,
    }
}

fn split_lines(bytes: &[u8], count: usize) -> Result<Vec<Line>, LineDiffError> {
    let mut lines = Vec::new();
    lines
        .try_reserve_exact(count)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    let mut start = 0;
    for (offset, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            lines.push(Line {
                start,
                end: offset + 1,
                fingerprint: fingerprint(&bytes[start..offset + 1]),
            });
            start = offset + 1;
        }
    }
    if start < bytes.len() {
        lines.push(Line {
            start,
            end: bytes.len(),
            fingerprint: fingerprint(&bytes[start..]),
        });
    }
    Ok(lines)
}

fn fingerprint(bytes: &[u8]) -> u64 {
    // FNV-1a is used only as a rejection filter; equality always verifies bytes.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn line_bytes(bytes: &[u8], line: Line) -> &[u8] {
    &bytes[line.start..line.end]
}

fn lines_equal(before: &[u8], left: Line, after: &[u8], right: Line) -> bool {
    left.fingerprint == right.fingerprint && line_bytes(before, left) == line_bytes(after, right)
}

/// Record extrema of optimal-LCS match candidates by their zero-based rank.
///
/// Rolling prefix rows avoid a second full DP table. The output is flat and
/// bounded by the number of lines; no per-candidate storage is needed.
fn possible_matches_by_rank(
    before: &[u8],
    after: &[u8],
    bl: &[Line],
    al: &[Line],
    suffix: &[usize],
    cols: usize,
    lcs_len: usize,
) -> Result<Vec<RankPossible>, LineDiffError> {
    let mut previous = Vec::new();
    let mut current = Vec::new();
    previous
        .try_reserve_exact(cols)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    current
        .try_reserve_exact(cols)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    previous.resize(cols, 0usize);
    current.resize(cols, 0usize);
    let mut possible_by_rank = Vec::new();
    possible_by_rank
        .try_reserve_exact(lcs_len)
        .map_err(|_| LineDiffError::AllocationFailed)?;
    possible_by_rank.resize(
        lcs_len,
        RankPossible {
            count: 0,
            before_min: usize::MAX,
            before_max: 0,
            after_min: usize::MAX,
            after_max: 0,
        },
    );
    for i in 0..bl.len() {
        current[0] = 0;
        for j in 0..al.len() {
            let equal = lines_equal(before, bl[i], after, al[j]);
            if equal && previous[j] + 1 + suffix[(i + 1) * cols + j + 1] == lcs_len {
                let rank = previous[j];
                let possible = &mut possible_by_rank[rank];
                possible.count = possible
                    .count
                    .checked_add(1)
                    .ok_or(LineDiffError::LimitOverflow)?;
                possible.before_min = possible.before_min.min(i);
                possible.before_max = possible.before_max.max(i);
                possible.after_min = possible.after_min.min(j);
                possible.after_max = possible.after_max.max(j);
            }
            let diagonal = if equal { previous[j] + 1 } else { 0 };
            current[j + 1] = previous[j + 1].max(current[j]).max(diagonal);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(possible_by_rank)
}

fn build_result(
    before_len: usize,
    after_len: usize,
    bl: &[Line],
    al: &[Line],
    matches: &[(usize, usize)],
    possible_by_rank: &[RankPossible],
) -> LineDiff {
    let mut equal = Vec::new();
    let mut hunks = Vec::new();
    let (mut before_cursor, mut after_cursor) = (0, 0);
    let mut k = 0;
    while k < matches.len() {
        let (bi, ai) = matches[k];
        let (mut bend, mut aend) = (bl[bi].end, al[ai].end);
        let mut next = k + 1;
        while next < matches.len()
            && matches[next].0 == matches[next - 1].0 + 1
            && matches[next].1 == matches[next - 1].1 + 1
        {
            bend = bl[matches[next].0].end;
            aend = al[matches[next].1].end;
            next += 1;
        }
        let bstart = bl[bi].start;
        let astart = al[ai].start;
        if before_cursor != bstart || after_cursor != astart {
            let ambiguous = boundary_is_ambiguous(possible_by_rank, k.checked_sub(1), Some(k));
            let (before_envelope, after_envelope) = hunk_envelope(
                before_cursor..bstart,
                after_cursor..astart,
                bl,
                al,
                possible_by_rank,
                k.checked_sub(1),
                Some(k),
                before_len,
                after_len,
            );
            hunks.push(LineHunk {
                before: before_cursor..bstart,
                after: after_cursor..astart,
                before_envelope,
                after_envelope,
                ambiguous,
            });
        }
        equal.push(EqualSpan {
            before: bstart..bend,
            after: astart..aend,
        });
        before_cursor = bend;
        after_cursor = aend;
        k = next;
    }
    if before_cursor != before_len || after_cursor != after_len {
        let left_rank = matches.len().checked_sub(1);
        let ambiguous = boundary_is_ambiguous(possible_by_rank, left_rank, None);
        let (before_envelope, after_envelope) = hunk_envelope(
            before_cursor..before_len,
            after_cursor..after_len,
            bl,
            al,
            possible_by_rank,
            left_rank,
            None,
            before_len,
            after_len,
        );
        hunks.push(LineHunk {
            before: before_cursor..before_len,
            after: after_cursor..after_len,
            before_envelope,
            after_envelope,
            ambiguous,
        });
    }
    LineDiff { equal, hunks }
}

fn boundary_is_ambiguous(
    possible_by_rank: &[RankPossible],
    left_rank: Option<usize>,
    right_rank: Option<usize>,
) -> bool {
    left_rank.is_some_and(|rank| possible_by_rank[rank].count != 1)
        || right_rank.is_some_and(|rank| possible_by_rank[rank].count != 1)
}

/// Expand a display hunk to all coordinates admitted by its adjacent uncertain
/// LCS ranks. The outer edge is fixed at the file edge when there is no rank.
/// These extrema are invariant across all optimal alignments.
#[allow(clippy::too_many_arguments)]
fn hunk_envelope(
    before: Range<usize>,
    after: Range<usize>,
    bl: &[Line],
    al: &[Line],
    possible: &[RankPossible],
    left: Option<usize>,
    right: Option<usize>,
    before_len: usize,
    after_len: usize,
) -> (Range<usize>, Range<usize>) {
    let bs = left.map_or(0, |r| {
        let p = possible[r];
        if p.count == 1 {
            bl[p.before_min].end
        } else {
            bl[p.before_min].start
        }
    });
    let as_ = left.map_or(0, |r| {
        let p = possible[r];
        if p.count == 1 {
            al[p.after_min].end
        } else {
            al[p.after_min].start
        }
    });
    let be = right.map_or(before_len, |r| {
        let p = possible[r];
        if p.count == 1 {
            bl[p.before_max].start
        } else {
            bl[p.before_max].end
        }
    });
    let ae = right.map_or(after_len, |r| {
        let p = possible[r];
        if p.count == 1 {
            al[p.after_max].start
        } else {
            al[p.after_max].end
        }
    });
    // An uncertain anchor is part of the component: in another alignment it
    // can move from one side of the edit to the other. Include it rather than
    // selecting either provenance.
    (
        bs.min(before.start)..be.max(before.end),
        as_.min(after.start)..ae.max(after.end),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(before: &[u8], after: &[u8]) -> LineDiff {
        line_diff(before, after, LineDiffLimits::default()).unwrap()
    }

    #[test]
    fn insertion_deletion_and_replacement_have_byte_ranges() {
        let inserted = diff(b"a\nc\n", b"a\nb\nc\n");
        assert_eq!(inserted.hunks[0].before, 2..2);
        assert_eq!(inserted.hunks[0].after, 2..4);
        let deleted = diff(b"a\nb\nc\n", b"a\nc\n");
        assert_eq!(deleted.hunks[0].before, 2..4);
        assert_eq!(deleted.hunks[0].after, 2..2);
        let replaced = diff(b"a\nbb\nc\n", "a\n世界\nc\n".as_bytes());
        assert_eq!(replaced.hunks[0].before, 2..5);
        assert_eq!(replaced.hunks[0].after, 2..9);
        assert!(!replaced.hunks[0].ambiguous);
    }

    #[test]
    fn bounded_binary_line_corpus_reconstructs_exact_after_bytes() {
        let mut inputs = Vec::new();
        for length in 0..=4 {
            for bits in 0..(1usize << length) {
                let mut text = Vec::new();
                for index in 0..length {
                    text.extend_from_slice(if bits & (1 << index) == 0 {
                        b"a\n"
                    } else {
                        b"b\n"
                    });
                }
                inputs.push(text);
            }
        }
        for before in &inputs {
            for after in &inputs {
                let result = diff(before, after);
                let mut rebuilt = Vec::new();
                let mut cursor = 0;
                for hunk in &result.hunks {
                    assert!(cursor <= hunk.before.start);
                    rebuilt.extend_from_slice(&before[cursor..hunk.before.start]);
                    rebuilt.extend_from_slice(&after[hunk.after.clone()]);
                    cursor = hunk.before.end;
                }
                rebuilt.extend_from_slice(&before[cursor..]);
                assert_eq!(&rebuilt, after);
                for span in &result.equal {
                    assert_eq!(&before[span.before.clone()], &after[span.after.clone()]);
                }
            }
        }
    }

    #[test]
    fn repeated_line_change_reports_ambiguity() {
        let d = diff(b"x\nx\n", b"x\nx\nx\n");
        assert_eq!(d.hunks.len(), 1);
        assert!(d.hunks[0].ambiguous);
        // Both identical source lines can border the insertion. The envelope
        // deliberately includes both rather than selecting the display match.
        assert_eq!(d.hunks[0].before_envelope, 2..4);
        assert_eq!(d.hunks[0].after_envelope, 2..6);
    }

    #[test]
    fn unrelated_ambiguous_separator_does_not_taint_early_hunk() {
        let d = diff(
            b"head\nold\nmiddle\n\nend\n",
            b"head\nnew\nmiddle\n\n\nend\n",
        );
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.hunks[0].before, 5..9);
        assert!(!d.hunks[0].ambiguous);
        assert!(d.hunks[1].ambiguous);
    }

    #[test]
    fn identical_repeated_lines_take_sound_fast_path() {
        let d = diff(b"x\nx\n", b"x\nx\n");
        assert!(d.hunks.is_empty());
        assert_eq!(
            d.equal,
            vec![EqualSpan {
                before: 0..4,
                after: 0..4
            }]
        );
    }

    #[test]
    fn final_newline_is_content() {
        let d = diff(b"x\n", b"x");
        assert_eq!(
            d.hunks,
            vec![LineHunk {
                before: 0..2,
                after: 0..1,
                before_envelope: 0..2,
                after_envelope: 0..1,
                ambiguous: false
            }]
        );
        assert!(d.equal.is_empty());
    }

    #[test]
    fn utf8_offsets_and_validation() {
        let d = diff("é\na\n".as_bytes(), "é\nb\n".as_bytes());
        assert_eq!(
            d.equal[0],
            EqualSpan {
                before: 0..3,
                after: 0..3
            }
        );
        assert_eq!(d.hunks[0].before, 3..5);
        assert_eq!(
            line_diff(&[0xff], b"", LineDiffLimits::default()),
            Err(LineDiffError::NonUtf8 {
                input: Input::Before
            })
        );
    }

    #[test]
    fn empty_inputs() {
        assert_eq!(
            diff(b"", b""),
            LineDiff {
                equal: vec![],
                hunks: vec![]
            }
        );
        assert_eq!(diff(b"", b"a\n").hunks[0].after, 0..2);
    }

    #[test]
    fn sparse_change_in_large_file_only_budgets_the_unanchored_middle() {
        let mut before = String::new();
        let mut after = String::new();
        for index in 0..3_000 {
            let line = format!("unique line {index}\n");
            before.push_str(&line);
            after.push_str(&line);
        }
        let needle = "unique line 1500\n";
        let offset = after.find(needle).unwrap();
        after.replace_range(offset..offset + needle.len(), "changed line\n");
        let result = line_diff(
            before.as_bytes(),
            after.as_bytes(),
            LineDiffLimits {
                max_cells: 16,
                max_work: 32,
                ..LineDiffLimits::default()
            },
        )
        .unwrap();
        assert_eq!(result.hunks.len(), 1);
        assert_eq!(&before[result.hunks[0].before.clone()], needle);
        assert_eq!(&after[result.hunks[0].after.clone()], "changed line\n");
    }

    #[test]
    fn repeated_equal_boundaries_remain_inside_ambiguity_analysis() {
        let result = line_diff(
            b"anchor\nrepeat\nold\nrepeat\ntail\n",
            b"anchor\nrepeat\nnew\nrepeat\ntail\n",
            LineDiffLimits {
                max_cells: 16,
                max_work: 32,
                ..LineDiffLimits::default()
            },
        )
        .unwrap();
        assert_eq!(result.hunks.len(), 1);
        assert_eq!(result.hunks[0].before, 14..18);
        assert_eq!(result.hunks[0].after, 14..18);
    }

    #[test]
    fn budgets_are_checked_before_dp_allocation() {
        let mut limits = LineDiffLimits {
            max_cells: 3,
            ..LineDiffLimits::default()
        };
        assert_eq!(
            line_diff(b"a\n", b"b\n", limits),
            Err(LineDiffError::CellBudgetExceeded {
                required: 4,
                limit: 3
            })
        );
        limits.max_cells = 4;
        limits.max_work = 3;
        assert_eq!(
            line_diff(b"a\n", b"b\n", limits),
            Err(LineDiffError::WorkBudgetExceeded {
                required: 4,
                limit: 3
            })
        );
    }
}
