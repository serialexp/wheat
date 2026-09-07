use super::matcher::match_parts;
use super::{
    CaptureRequest, DEFAULT_EXTRA_CONTEXT_LINES, Error, GuardedEdit, Limits, MatchStatus,
    validate_fragment, validate_text,
};

/// Capture an edit from canonical LF UTF-8 text. Request offsets are canonical
/// byte offsets; line-ending normalization belongs at the external capture boundary.
pub fn capture(
    canonical: &[u8],
    request: CaptureRequest,
    replacement: &[u8],
    limits: Limits,
) -> Result<GuardedEdit, Error> {
    validate_text(canonical, limits)?;
    validate_fragment(replacement, limits)?;
    validate_range(canonical, &request.replacement)?;
    if let Some(wider) = &request.wider_guard {
        validate_range(canonical, wider)?;
        if wider.start > request.replacement.start || wider.end < request.replacement.end {
            return Err(Error::ExplicitGuardDoesNotContainReplacement);
        }
    }

    let rs = request.replacement.start;
    let re = request.replacement.end;
    let mut gs = rs;
    let mut ge = re;
    let mut work = 0usize;

    // First determine the default unique symmetric context. The explicit range
    // is only a lower bound on the final coverage, not the starting point for
    // uniqueness or the two-line safety margin.
    loop {
        match match_parts(
            canonical,
            &canonical[gs..rs],
            &canonical[rs..re],
            &canonical[re..ge],
            gs == 0,
            ge == canonical.len(),
            limits,
            &mut work,
        )? {
            MatchStatus::Unique(_) => break,
            MatchStatus::NoMatch => unreachable!("the source range always matches its source"),
            MatchStatus::Ambiguous => {}
        }
        let next_s = grow_left(canonical, gs);
        let next_e = grow_right(canonical, ge);
        if next_s == gs && next_e == ge {
            return Err(Error::CaptureAmbiguous);
        }
        gs = next_s;
        ge = next_e;
    }

    for _ in 0..DEFAULT_EXTRA_CONTEXT_LINES {
        gs = grow_left(canonical, gs);
        ge = grow_right(canonical, ge);
    }
    if let Some(wider) = &request.wider_guard {
        gs = gs.min(wider.start);
        ge = ge.max(wider.end);
    }

    // Recheck the completed guard so its actual size and scan are both covered
    // by the same aggregate capture work budget.
    match match_parts(
        canonical,
        &canonical[gs..rs],
        &canonical[rs..re],
        &canonical[re..ge],
        gs == 0,
        ge == canonical.len(),
        limits,
        &mut work,
    )? {
        MatchStatus::Unique(_) => {}
        MatchStatus::Ambiguous | MatchStatus::NoMatch => return Err(Error::CaptureAmbiguous),
    }

    Ok(GuardedEdit {
        before: canonical[gs..rs].to_vec(),
        expected: canonical[rs..re].to_vec(),
        replacement: replacement.to_vec(),
        after: canonical[re..ge].to_vec(),
        start_anchor: gs == 0,
        end_anchor: ge == canonical.len(),
    })
}

fn validate_range(bytes: &[u8], range: &std::ops::Range<usize>) -> Result<(), Error> {
    if range.start > range.end || range.end > bytes.len() {
        return Err(Error::InvalidRange);
    }
    let text = std::str::from_utf8(bytes).expect("validated text");
    if !text.is_char_boundary(range.start) {
        return Err(Error::NotUtf8Boundary {
            offset: range.start,
        });
    }
    if !text.is_char_boundary(range.end) {
        return Err(Error::NotUtf8Boundary { offset: range.end });
    }
    Ok(())
}

fn grow_left(canonical: &[u8], position: usize) -> usize {
    if position == 0 {
        0
    } else {
        canonical[..position.saturating_sub(1)]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |offset| offset + 1)
    }
}

fn grow_right(canonical: &[u8], position: usize) -> usize {
    if position >= canonical.len() {
        canonical.len()
    } else {
        canonical[position..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(canonical.len(), |offset| position + offset + 1)
    }
}
