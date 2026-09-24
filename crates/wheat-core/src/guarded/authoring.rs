//! Pure incremental normalization of guarded edits.
//!
//! This module deliberately does not assign edit IDs.  An unaffected
//! [`GuardedEdit`] is cloned verbatim; callers which key identity by the complete
//! definition can therefore retain that identity.  Only components reached by a
//! workspace change (or by guard/write interference after recapture) are rebuilt.

use std::ops::Range;

use super::line_diff::{LineDiff, LineDiffError, LineDiffLimits, line_diff};
use super::{CaptureRequest, EditOutcome, Error, GuardedEdit, Limits, apply, capture};

/// Failure to prove an exact incremental normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthoringError {
    /// Validation, application, or capture failed.
    Guarded(Error),
    /// The bounded line diff failed.
    Diff(LineDiffError),
    /// Applying `existing` to the authored base did not produce `baseline`.
    BaselineMismatch,
    /// An existing definition did not apply independently and exactly.
    ExistingNotApplied { index: usize, outcome: EditOutcome },
    /// A repeated-line diff has no unique coordinate mapping.  The normalizer
    /// refuses to use the diff's deterministic display tie-break as provenance.
    AmbiguousMapping { hunk: usize },
    /// A requested wider guard was invalid or could not be related uniquely to
    /// the changed component.  Explicit ranges are workspace byte ranges.
    InvalidExplicitGuard { index: usize },
    /// Pair, component, and capture traversal exceeded the shared authoring
    /// budget. Diff and matcher/capture enforce their own allocations as well.
    WorkBudgetExceeded { limit: usize },
    /// The constructed definitions did not reproduce the supplied workspace.
    VerificationFailed,
}

impl From<Error> for AuthoringError {
    fn from(value: Error) -> Self {
        Self::Guarded(value)
    }
}
impl From<LineDiffError> for AuthoringError {
    fn from(value: LineDiffError) -> Self {
        Self::Diff(value)
    }
}

#[derive(Clone, Debug)]
struct Located {
    index: usize,
    read_a: Range<usize>,
    write_a: Range<usize>,
    write_b: Range<usize>,
}

#[derive(Clone, Debug)]
struct Component {
    write: Range<usize>,
    wider: Range<usize>,
    members: Vec<usize>,
}

/// A final fixed-point normalization component.
///
/// `members` are indices into the old `existing` input. `replacement` is absent
/// when the component is a net undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedComponent {
    pub members: Vec<usize>,
    pub replacement: Option<GuardedEdit>,
}

/// Normalized definitions together with the lineage of rebuilt components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizeResult {
    pub edits: Vec<GuardedEdit>,
    pub components: Vec<NormalizedComponent>,
}

/// Normalize changes accumulated between `baseline` and `workspace` against the
/// common `authored` base.
///
/// `explicit_guard_ranges`, when present, contains workspace-coordinate ranges
/// selected while adding the new changes.  Each must contain (or touch, for an
/// insertion) at least one changed hunk.  Existing affected edits always retain
/// their complete old read range as a lower bound because an old definition does
/// not record which part of its context was explicitly requested.
///
/// Work is bounded by `limits.max_work`: line diff receives that bound, pair and
/// fixed-point traversals are charged here, and no more than `limits.max_edits`
/// captures are attempted.  Capture itself remains subject to `Limits`.
pub fn normalize(
    authored: &[u8],
    existing: &[GuardedEdit],
    baseline: &[u8],
    workspace: &[u8],
    explicit_guard_ranges: Option<&[Range<usize>]>,
    limits: Limits,
) -> Result<Vec<GuardedEdit>, AuthoringError> {
    Ok(normalize_with_lineage(
        authored,
        existing,
        baseline,
        workspace,
        explicit_guard_ranges,
        limits,
    )?
    .edits)
}

/// Normalize changes and report each rebuilt final fixed-point component.
pub fn normalize_with_lineage(
    authored: &[u8],
    existing: &[GuardedEdit],
    baseline: &[u8],
    workspace: &[u8],
    explicit_guard_ranges: Option<&[Range<usize>]>,
    limits: Limits,
) -> Result<NormalizeResult, AuthoringError> {
    if existing.len() > limits.max_edits {
        return Err(Error::TooManyEdits {
            actual: existing.len(),
            limit: limits.max_edits,
        }
        .into());
    }
    let applied = apply(authored, existing, limits)?;
    for (index, outcome) in applied.outcomes.iter().enumerate() {
        if !matches!(outcome, EditOutcome::Applied { .. }) {
            return Err(AuthoringError::ExistingNotApplied {
                index,
                outcome: outcome.clone(),
            });
        }
    }
    if applied.content != baseline {
        return Err(AuthoringError::BaselineMismatch);
    }

    let mut located = Vec::with_capacity(existing.len());
    for (index, outcome) in applied.outcomes.iter().enumerate() {
        let EditOutcome::Applied { read, write } = outcome else {
            unreachable!()
        };
        located.push(Located {
            index,
            read_a: read.clone(),
            write_a: write.clone(),
            write_b: 0..0,
        });
    }
    located.sort_by_key(|v| (v.write_a.start, v.index));
    let mut delta: isize = 0;
    for item in &mut located {
        let bs = shift(item.write_a.start, delta).ok_or(AuthoringError::BaselineMismatch)?;
        let be = bs
            .checked_add(existing[item.index].replacement.len())
            .ok_or(AuthoringError::BaselineMismatch)?;
        item.write_b = bs..be;
        delta = delta
            .checked_add(existing[item.index].replacement.len() as isize)
            .and_then(|v| v.checked_sub(item.write_a.len() as isize))
            .ok_or(AuthoringError::BaselineMismatch)?;
    }

    let diff_limits = LineDiffLimits {
        max_input_bytes: limits.max_input_bytes,
        max_work: limits.max_work,
        ..LineDiffLimits::default()
    };
    let diff = line_diff(baseline, workspace, diff_limits)?;
    if diff.hunks.is_empty() {
        return Ok(NormalizeResult {
            edits: existing.to_vec(),
            components: Vec::new(),
        });
    }

    let mut work = 0usize;
    let mut dirty = vec![false; existing.len()];
    for h in &diff.hunks {
        // An ambiguous display boundary is never used as provenance. Its
        // conservative envelope instead includes every coordinate that can be
        // crossed by an optimal alignment, so the authored endpoints are
        // invariant across tie-breaks.
        for item in &located {
            charge(&mut work, 1, limits)?;
            let read_b = project_a_range(&item.read_a, &located, existing)?;
            if change_interferes_with_read(
                &h.before_envelope,
                &read_b,
                existing[item.index].start_anchor,
                existing[item.index].end_anchor,
            ) {
                dirty[item.index] = true;
            }
        }
    }

    let mut components = Vec::new();
    for (hi, h) in diff.hunks.iter().enumerate() {
        let mut write = reverse_envelope(&h.before_envelope, &located, existing)
            .ok_or(AuthoringError::AmbiguousMapping { hunk: hi })?;
        let mut members = Vec::new();
        for item in &located {
            if dirty[item.index]
                && change_interferes_with_read(
                    &h.before_envelope,
                    &project_a_range(&item.read_a, &located, existing)?,
                    existing[item.index].start_anchor,
                    existing[item.index].end_anchor,
                )
            {
                write.start = write.start.min(item.write_a.start);
                write.end = write.end.max(item.write_a.end);
                members.push(item.index);
            }
        }
        components.push(Component {
            wider: write.clone(),
            write,
            members,
        });
    }
    // An affected edit may be reached by multiple hunks; merge those components.
    merge_components(&mut components, &mut work, limits)?;
    for component in &mut components {
        for item in &located {
            if component.members.contains(&item.index) {
                component.wider.start = component.wider.start.min(item.read_a.start);
                component.wider.end = component.wider.end.max(item.read_a.end);
            }
        }
    }

    if let Some(ranges) = explicit_guard_ranges {
        let text =
            std::str::from_utf8(workspace).map_err(|_| AuthoringError::Guarded(Error::NonUtf8))?;
        for (ri, range) in ranges.iter().enumerate() {
            if range.start > range.end
                || range.end > workspace.len()
                || !text.is_char_boundary(range.start)
                || !text.is_char_boundary(range.end)
            {
                return Err(AuthoringError::InvalidExplicitGuard { index: ri });
            }
            let mut attached = false;
            for (hi, h) in diff.hunks.iter().enumerate() {
                if changed_touches(&h.after, range) {
                    let b = reverse_diff_range(range, &diff)
                        .ok_or(AuthoringError::AmbiguousMapping { hunk: hi })?;
                    let a = reverse_envelope(&b, &located, existing)
                        .ok_or(AuthoringError::AmbiguousMapping { hunk: hi })?;
                    for c in &mut components {
                        if ranges_touch(
                            &c.write,
                            &reverse_envelope(&h.before, &located, existing).unwrap(),
                        ) {
                            c.wider.start = c.wider.start.min(a.start);
                            c.wider.end = c.wider.end.max(a.end);
                            attached = true;
                        }
                    }
                }
            }
            if !attached {
                return Err(AuthoringError::InvalidExplicitGuard { index: ri });
            }
        }
    }

    let mut captures: Vec<Option<GuardedEdit>> = Vec::new();
    loop {
        merge_components(&mut components, &mut work, limits)?;
        captures.clear();
        for c in &components {
            charge(&mut work, 1, limits)?;
            let b = project_a_range(&c.write, &located, existing)?;
            let w = forward_diff_range(&b, &diff)
                .ok_or(AuthoringError::AmbiguousMapping { hunk: 0 })?;
            if authored[c.write.clone()] == workspace[w.clone()] {
                captures.push(None); // net undo
            } else {
                captures.push(Some(capture(
                    authored,
                    CaptureRequest {
                        replacement: c.write.clone(),
                        wider_guard: Some(c.wider.clone()),
                    },
                    &workspace[w],
                    limits,
                )?));
            }
        }

        let mut changed = false;
        // New diff hunks may only interfere after capture grows their guards.
        // Merge them at this fixed point rather than treating a renderer's hunk
        // partition as canonical edit identity.
        'new_components: for i in 0..components.len() {
            for j in i + 1..components.len() {
                charge(&mut work, 1, limits)?;
                let i_reaches_j = captures[i].as_ref().is_some_and(|edit| {
                    write_touches_read(
                        &components[j].write,
                        &captured_read(&components[i].write, edit),
                    )
                });
                let j_reaches_i = captures[j].as_ref().is_some_and(|edit| {
                    write_touches_read(
                        &components[i].write,
                        &captured_read(&components[j].write, edit),
                    )
                });
                if i_reaches_j || j_reaches_i {
                    merge_component_pair(&mut components, i, j);
                    changed = true;
                    break 'new_components;
                }
            }
        }
        if changed {
            continue;
        }

        'outer: for ci in 0..components.len() {
            let Some(edit) = &captures[ci] else { continue };
            let read = captured_read(&components[ci].write, edit);
            for item in &located {
                if components[ci].members.contains(&item.index) {
                    continue;
                }
                charge(&mut work, 1, limits)?;
                if write_touches_read(&item.write_a, &read) {
                    components[ci].write.start = components[ci].write.start.min(item.write_a.start);
                    components[ci].write.end = components[ci].write.end.max(item.write_a.end);
                    components[ci].wider.start = components[ci].wider.start.min(item.read_a.start);
                    components[ci].wider.end = components[ci].wider.end.max(item.read_a.end);
                    components[ci].members.push(item.index);
                    dirty[item.index] = true;
                    changed = true;
                    break 'outer;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Components and their captures are now at the final fixed point.  Preserve
    // this pairing before filtering net undos from the returned definitions.
    let normalized_components: Vec<_> = components
        .iter()
        .zip(&captures)
        .map(|(component, replacement)| NormalizedComponent {
            members: component.members.clone(),
            replacement: replacement.clone(),
        })
        .collect();

    let mut result: Vec<(usize, usize, GuardedEdit)> = Vec::new();
    for item in &located {
        if !dirty[item.index] {
            result.push((item.write_a.start, item.index, existing[item.index].clone()));
        }
    }
    for (n, (component, captured)) in components.iter().zip(captures).enumerate() {
        if let Some(edit) = captured {
            result.push((component.write.start, existing.len() + n, edit));
        }
    }
    result.sort_by_key(|(start, tie, _)| (*start, *tie));
    let result: Vec<_> = result.into_iter().map(|(_, _, edit)| edit).collect();
    if result.len() > limits.max_edits {
        return Err(Error::TooManyEdits {
            actual: result.len(),
            limit: limits.max_edits,
        }
        .into());
    }
    let verification = apply(authored, &result, limits)?;
    if verification.content != workspace
        || verification
            .outcomes
            .iter()
            .any(|o| !matches!(o, EditOutcome::Applied { .. }))
    {
        return Err(AuthoringError::VerificationFailed);
    }
    Ok(NormalizeResult {
        edits: result,
        components: normalized_components,
    })
}

fn charge(work: &mut usize, amount: usize, limits: Limits) -> Result<(), AuthoringError> {
    *work = work
        .checked_add(amount)
        .ok_or(AuthoringError::WorkBudgetExceeded {
            limit: limits.max_work,
        })?;
    if *work > limits.max_work {
        return Err(AuthoringError::WorkBudgetExceeded {
            limit: limits.max_work,
        });
    }
    Ok(())
}
fn shift(value: usize, delta: isize) -> Option<usize> {
    if delta >= 0 {
        value.checked_add(delta as usize)
    } else {
        value.checked_sub(delta.unsigned_abs())
    }
}
fn changed_touches(change: &Range<usize>, area: &Range<usize>) -> bool {
    if change.is_empty() {
        area.start <= change.start && change.start <= area.end
    } else {
        change.start < area.end && area.start < change.end
    }
}

/// A boundary anchor is guard material too. An insertion immediately outside the
/// captured bytes invalidates the asserted file boundary and must rebuild the edit.
fn change_interferes_with_read(
    change: &Range<usize>,
    read: &Range<usize>,
    start_anchor: bool,
    end_anchor: bool,
) -> bool {
    changed_touches(change, read)
        || (change.is_empty()
            && ((start_anchor && change.start == read.start)
                || (end_anchor && change.start == read.end)))
}

fn ranges_touch(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start <= b.end && b.start <= a.end
}
fn write_touches_read(write: &Range<usize>, read: &Range<usize>) -> bool {
    if write.is_empty() {
        read.start <= write.start && write.start <= read.end
    } else {
        write.start < read.end && read.start < write.end
    }
}

fn project_boundary_a(
    p: usize,
    loc: &[Located],
    edits: &[GuardedEdit],
    right: bool,
) -> Option<usize> {
    let mut delta = 0isize;
    for x in loc {
        if p < x.write_a.start || (p == x.write_a.start && !right) {
            break;
        }
        if p <= x.write_a.end {
            return Some(if p == x.write_a.start {
                x.write_b.start
            } else {
                x.write_b.end
            });
        }
        delta = delta
            .checked_add(edits[x.index].replacement.len() as isize)?
            .checked_sub(x.write_a.len() as isize)?;
    }
    shift(p, delta)
}
fn project_a_range(
    r: &Range<usize>,
    loc: &[Located],
    edits: &[GuardedEdit],
) -> Result<Range<usize>, AuthoringError> {
    Ok(
        project_boundary_a(r.start, loc, edits, false).ok_or(AuthoringError::BaselineMismatch)?
            ..project_boundary_a(r.end, loc, edits, true)
                .ok_or(AuthoringError::BaselineMismatch)?,
    )
}
fn reverse_envelope(
    r: &Range<usize>,
    loc: &[Located],
    edits: &[GuardedEdit],
) -> Option<Range<usize>> {
    let mut points = Vec::with_capacity(2 + loc.len() * 2);
    points.push(reverse_boundary_b(r.start, loc, edits, false)?);
    points.push(reverse_boundary_b(r.end, loc, edits, true)?);
    for x in loc {
        if changed_touches(r, &x.write_b) {
            points.push(x.write_a.start);
            points.push(x.write_a.end);
        }
    }
    Some(*points.iter().min()?..*points.iter().max()?)
}
/// Maps a baseline boundary to the authored coordinate that conservatively bounds it.
///
/// A boundary strictly inside an existing edit's written bytes has no authored
/// counterpart: those bytes exist only in the replacement. It therefore expands to the
/// whole authored range that edit replaces, the start for a left boundary and the end
/// for a right one, so a change inside a replacement consolidates with that edit.
fn reverse_boundary_b(
    p: usize,
    loc: &[Located],
    edits: &[GuardedEdit],
    right: bool,
) -> Option<usize> {
    let mut delta = 0isize;
    for x in loc {
        if p < x.write_b.start || (p == x.write_b.start && !right) {
            break;
        }
        if p <= x.write_b.end {
            return Some(if p == x.write_b.start {
                x.write_a.start
            } else if p == x.write_b.end || right {
                x.write_a.end
            } else {
                x.write_a.start
            });
        }
        delta = delta
            .checked_add(edits[x.index].replacement.len() as isize)?
            .checked_sub(x.write_a.len() as isize)?;
    }
    shift(p, -delta)
}

fn map_diff_boundary(p: usize, diff: &LineDiff, forward: bool, right: bool) -> Option<usize> {
    let mut delta = 0isize;
    for h in &diff.hunks {
        let (from, to) = if forward {
            (&h.before, &h.after)
        } else {
            (&h.after, &h.before)
        };
        if p < from.start || (p == from.start && !right) {
            break;
        }
        if p <= from.end {
            return Some(if from.is_empty() && right {
                to.end
            } else if p == from.start {
                to.start
            } else if p == from.end {
                to.end
            } else {
                return None;
            });
        }
        delta = delta
            .checked_add(to.len() as isize)?
            .checked_sub(from.len() as isize)?;
    }
    shift(p, delta)
}
fn forward_diff_range(r: &Range<usize>, d: &LineDiff) -> Option<Range<usize>> {
    Some(map_diff_boundary(r.start, d, true, false)?..map_diff_boundary(r.end, d, true, true)?)
}
fn reverse_diff_range(r: &Range<usize>, d: &LineDiff) -> Option<Range<usize>> {
    Some(map_diff_boundary(r.start, d, false, false)?..map_diff_boundary(r.end, d, false, true)?)
}
fn captured_read(write: &Range<usize>, edit: &GuardedEdit) -> Range<usize> {
    write.start - edit.before.len()..write.end + edit.after.len()
}

fn merge_components(
    cs: &mut Vec<Component>,
    work: &mut usize,
    limits: Limits,
) -> Result<(), AuthoringError> {
    loop {
        let mut pair = None;
        'scan: for i in 0..cs.len() {
            for j in i + 1..cs.len() {
                charge(work, 1, limits)?;
                let shared_member = cs[i].members.iter().any(|m| cs[j].members.contains(m));
                if ranges_touch(&cs[i].write, &cs[j].write) || shared_member {
                    pair = Some((i, j));
                    break 'scan;
                }
            }
        }
        let Some((i, j)) = pair else { break };
        merge_component_pair(cs, i, j);
    }
    Ok(())
}

fn merge_component_pair(cs: &mut Vec<Component>, i: usize, j: usize) {
    debug_assert!(i < j);
    let other = cs.remove(j);
    cs[i].write.start = cs[i].write.start.min(other.write.start);
    cs[i].write.end = cs[i].write.end.max(other.write.end);
    cs[i].wider.start = cs[i].wider.start.min(other.wider.start);
    cs[i].wider.end = cs[i].wider.end.max(other.wider.end);
    cs[i].members.extend(other.members);
    cs[i].members.sort_unstable();
    cs[i].members.dedup();
}
