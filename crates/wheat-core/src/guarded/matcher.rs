use super::types::{
    Error, GuardedEdit, Limits, MatchStatus, MatchedGuard, WorkBudget, boundary, validate_fragment,
    validate_text,
};

pub fn match_guard(
    target: &[u8],
    edit: &GuardedEdit,
    limits: Limits,
) -> Result<MatchStatus, Error> {
    validate_text(target, limits)?;
    validate_fragment(&edit.before, limits)?;
    validate_fragment(&edit.expected, limits)?;
    validate_fragment(&edit.replacement, limits)?;
    validate_fragment(&edit.after, limits)?;
    let mut work = WorkBudget::new(limits.max_work);
    match_parts_budget(
        target,
        &edit.before,
        &edit.expected,
        &edit.after,
        edit.start_anchor,
        edit.end_anchor,
        limits,
        &mut work,
    )
}

/// Match already-validated borrowed guard fragments, charging a shared budget.
pub(crate) fn match_parts(
    target: &[u8],
    before: &[u8],
    expected: &[u8],
    after: &[u8],
    start_anchor: bool,
    end_anchor: bool,
    limits: Limits,
    work: &mut usize,
) -> Result<MatchStatus, Error> {
    let mut budget = WorkBudget::with_used(limits.max_work, *work);
    let result = match_parts_budget(
        target,
        before,
        expected,
        after,
        start_anchor,
        end_anchor,
        limits,
        &mut budget,
    );
    *work = limits.max_work.saturating_sub(budget.remaining());
    result
}

pub(crate) fn match_parts_budget(
    target: &[u8],
    before: &[u8],
    expected: &[u8],
    after: &[u8],
    start_anchor: bool,
    end_anchor: bool,
    limits: Limits,
    work: &mut WorkBudget,
) -> Result<MatchStatus, Error> {
    let plen = before
        .len()
        .checked_add(expected.len())
        .and_then(|n| n.checked_add(after.len()))
        .ok_or(Error::InputTooLarge {
            actual: usize::MAX,
            limit: limits.max_input_bytes,
        })?;
    if plen > limits.max_input_bytes {
        return Err(Error::InputTooLarge {
            actual: plen,
            limit: limits.max_input_bytes,
        });
    }
    let starts = if plen <= target.len() {
        target.len() - plen + 1
    } else {
        0
    };
    let (first, past_last) = if starts == 0 {
        (0, 0)
    } else if start_anchor {
        (0, 1)
    } else if end_anchor {
        (target.len() - plen, target.len() - plen + 1)
    } else {
        (0, starts)
    };
    let mut one = None;
    if first == past_last {
        return Ok(MatchStatus::NoMatch);
    }
    if plen == 0 {
        // Empty patterns match canonical text (UTF-8) boundaries, not arbitrary bytes.
        for start in first..past_last {
            if (start_anchor && start != 0)
                || (end_anchor && start != target.len())
                || boundary(target, start).is_err()
            {
                continue;
            }
            let found = MatchedGuard {
                read: start..start,
                write: start..start,
            };
            if one.is_some() {
                return Ok(MatchStatus::Ambiguous);
            }
            one = Some(found);
        }
        return Ok(one.map(MatchStatus::Unique).unwrap_or(MatchStatus::NoMatch));
    }

    if start_anchor || end_anchor {
        let start = first;
        if part_matches(target, start, before, work)?
            && part_matches(target, start + before.len(), expected, work)?
            && part_matches(target, start + before.len() + expected.len(), after, work)?
        {
            one = Some(matched_guard(start, plen, before.len(), expected.len()));
        }
        return Ok(one.map(MatchStatus::Unique).unwrap_or(MatchStatus::NoMatch));
    }

    // Boyer-Moore-Horspool skips alignments which cannot match based on their
    // final byte. The 256-entry table stays on the stack, and the fragmented
    // guard is compared in place instead of allocating a combined pattern.
    // Build and compare one fragment at a time: selecting a fragment for every
    // byte comparison was measurable in this hot loop.
    let mut shift = [plen; 256];
    let mut offset = 0;
    for part in [before, expected, after] {
        for &byte in part {
            if offset + 1 < plen {
                shift[byte as usize] = plen - 1 - offset;
            }
            offset += 1;
        }
    }
    let mut start = first;
    while start < past_last {
        let matches =
            reverse_part_matches(target, start + before.len() + expected.len(), after, work)?
                && reverse_part_matches(target, start + before.len(), expected, work)?
                && reverse_part_matches(target, start, before, work)?;
        if matches {
            let found = matched_guard(start, plen, before.len(), expected.len());
            if one.is_some() {
                return Ok(MatchStatus::Ambiguous);
            }
            one = Some(found);
        }
        start = start.saturating_add(shift[target[start + plen - 1] as usize]);
    }
    Ok(one.map(MatchStatus::Unique).unwrap_or(MatchStatus::NoMatch))
}

/// Compare one fragment from its end so the BMH rejection byte is examined first.
#[inline]
fn reverse_part_matches(
    target: &[u8],
    start: usize,
    part: &[u8],
    work: &mut WorkBudget,
) -> Result<bool, Error> {
    const WORD: usize = size_of::<u64>();
    let target = &target[start..start + part.len()];
    let mut offset = part.len();
    let mut compared = 0;

    // Compare full words from the end. When a word differs, leading zero bytes
    // in the little-endian XOR are precisely the bytes the scalar reverse scan
    // would have accepted before its first mismatch, preserving exact budget
    // accounting while avoiding one branch and charge call per equal byte.
    while offset >= WORD {
        let chunk_start = offset - WORD;
        let actual = u64::from_le_bytes(
            target[chunk_start..offset]
                .try_into()
                .expect("word-sized target chunk"),
        );
        let expected = u64::from_le_bytes(
            part[chunk_start..offset]
                .try_into()
                .expect("word-sized pattern chunk"),
        );
        let difference = actual ^ expected;
        if difference != 0 {
            compared += (difference.leading_zeros() as usize / 8) + 1;
            work.charge(compared)?;
            return Ok(false);
        }
        compared += WORD;
        offset = chunk_start;
    }
    while offset != 0 {
        offset -= 1;
        compared += 1;
        if target[offset] != part[offset] {
            work.charge(compared)?;
            return Ok(false);
        }
    }
    work.charge(compared)?;
    Ok(true)
}

fn matched_guard(
    start: usize,
    pattern_len: usize,
    before_len: usize,
    expected_len: usize,
) -> MatchedGuard {
    let write_start = start + before_len;
    MatchedGuard {
        read: start..start + pattern_len,
        write: write_start..write_start + expected_len,
    }
}

/// Compare a contiguous guard fragment without constructing a combined pattern.
/// One unit is charged for each pair of bytes examined, including a mismatch.
#[inline]
fn part_matches(
    target: &[u8],
    start: usize,
    part: &[u8],
    work: &mut WorkBudget,
) -> Result<bool, Error> {
    debug_assert!(start <= target.len());
    debug_assert!(part.len() <= target.len() - start);
    for (offset, &expected) in part.iter().enumerate() {
        // Reserve this comparison before loading either byte, so an exhausted
        // budget cannot inspect another candidate byte.
        work.charge(1)?;
        if target[start + offset] != expected {
            return Ok(false);
        }
    }
    Ok(true)
}
