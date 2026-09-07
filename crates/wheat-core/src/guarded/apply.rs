use std::ops::Range;

use super::matcher::match_parts_budget;
use super::types::{GuardedEditRef, WorkBudget, validate_fragment, validate_text};
use super::{ApplyResult, EditOutcome, Error, GuardedEdit, Limits, MatchStatus, MatchedGuard};

/// Apply owned edits with the traditional per-call work and output limits.
pub fn apply(target: &[u8], edits: &[GuardedEdit], limits: Limits) -> Result<ApplyResult, Error> {
    let borrowed: Vec<_> = edits.iter().map(GuardedEditRef::from).collect();
    let mut work = WorkBudget::new(limits.max_work);
    ApplyResult::apply_borrowed(target, &borrowed, limits, limits.max_input_bytes, &mut work)
}

/// Apply borrowed edits while charging an adapter-wide work budget.
///
/// `max_output_bytes` is separate from `Limits::max_input_bytes`: callers may accept an existing
/// source larger than the remaining generated-content allowance, but a changed output is rejected
/// before its backing allocation is made.
impl ApplyResult {
    pub(crate) fn apply_borrowed(
        target: &[u8],
        edits: &[GuardedEditRef<'_>],
        limits: Limits,
        max_output_bytes: usize,
        work: &mut WorkBudget,
    ) -> Result<Self, Error> {
        validate_text(target, limits)?;
        if edits.len() > limits.max_edits {
            return Err(Error::TooManyEdits {
                actual: edits.len(),
                limit: limits.max_edits,
            });
        }
        let mut matched: Vec<Option<MatchedGuard>> = Vec::with_capacity(edits.len());
        let mut outcomes = Vec::with_capacity(edits.len());
        for edit in edits {
            validate_fragment(edit.before, limits)?;
            validate_fragment(edit.expected, limits)?;
            validate_fragment(edit.replacement, limits)?;
            validate_fragment(edit.after, limits)?;
            match match_parts_budget(
                target,
                edit.before,
                edit.expected,
                edit.after,
                edit.start_anchor,
                edit.end_anchor,
                limits,
                work,
            )? {
                MatchStatus::Unique(m) => {
                    outcomes.push(EditOutcome::Applied {
                        read: m.read.clone(),
                        write: m.write.clone(),
                    });
                    matched.push(Some(m));
                }
                MatchStatus::NoMatch => {
                    outcomes.push(EditOutcome::NoMatch);
                    matched.push(None);
                }
                MatchStatus::Ambiguous => {
                    outcomes.push(EditOutcome::Ambiguous);
                    matched.push(None);
                }
            }
        }
        let mut conflicts = vec![Vec::new(); edits.len()];
        for i in 0..edits.len() {
            let Some(a) = &matched[i] else { continue };
            for j in i + 1..edits.len() {
                let Some(b) = &matched[j] else { continue };
                work.charge(1)?;
                if write_touches_read(&a.write, &b.read)
                    || write_touches_read(&b.write, &a.read)
                    || writes_overlap(&a.write, &b.write)
                {
                    conflicts[i].push(j);
                    conflicts[j].push(i);
                }
            }
        }
        for (i, with) in conflicts.into_iter().enumerate() {
            if !with.is_empty() {
                outcomes[i] = EditOutcome::Interference { with };
            }
        }
        let mut applicable: Vec<_> = matched
            .into_iter()
            .enumerate()
            .filter_map(|(i, matched)| {
                if matches!(outcomes[i], EditOutcome::Applied { .. }) {
                    matched.map(|value| (i, value))
                } else {
                    None
                }
            })
            .collect();
        applicable
            .sort_by(|(ai, a), (bi, b)| a.write.start.cmp(&b.write.start).then_with(|| ai.cmp(bi)));
        let result_len = applicable
            .iter()
            .try_fold(target.len(), |length, (i, matched)| {
                length
                    .checked_sub(matched.write.len())
                    .and_then(|n| n.checked_add(edits[*i].replacement.len()))
                    .ok_or(Error::InputTooLarge {
                        actual: usize::MAX,
                        limit: limits.max_input_bytes,
                    })
            })?;
        if result_len > limits.max_input_bytes {
            return Err(Error::InputTooLarge {
                actual: result_len,
                limit: limits.max_input_bytes,
            });
        }
        let changed = applicable
            .iter()
            .any(|(i, matched)| target[matched.write.clone()] != *edits[*i].replacement);
        if changed && result_len > max_output_bytes {
            return Err(Error::OutputTooLarge {
                actual: result_len,
                limit: max_output_bytes,
            });
        }
        let mut content = Vec::with_capacity(result_len);
        let mut cursor = 0;
        for (i, matched) in applicable {
            content.extend_from_slice(&target[cursor..matched.write.start]);
            content.extend_from_slice(edits[i].replacement);
            cursor = matched.write.end;
        }
        content.extend_from_slice(&target[cursor..]);
        debug_assert_eq!(content.len(), result_len);
        Ok(ApplyResult { content, outcomes })
    }
}

fn write_touches_read(write: &Range<usize>, read: &Range<usize>) -> bool {
    if write.is_empty() {
        read.start <= write.start && write.start <= read.end
    } else {
        write.start < read.end && read.start < write.end
    }
}

fn writes_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() && b.is_empty() {
        a.start == b.start
    } else if a.is_empty() {
        b.start <= a.start && a.start < b.end
    } else if b.is_empty() {
        a.start <= b.start && b.start < a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}
