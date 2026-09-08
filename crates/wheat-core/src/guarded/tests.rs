use super::*;

fn g(before: &str, expected: &str, replacement: &str, after: &str) -> GuardedEdit {
    GuardedEdit {
        before: before.into(),
        expected: expected.into(),
        replacement: replacement.into(),
        after: after.into(),
        start_anchor: false,
        end_anchor: false,
    }
}

#[test]
fn exact_unique_moved_missing_and_ambiguous() {
    let e = g("a", "b", "X", "c");
    assert!(matches!(
        match_guard(b"moved abc here", &e, Limits::default()).unwrap(),
        MatchStatus::Unique(_)
    ));
    assert_eq!(
        match_guard(b"a b c", &e, Limits::default()).unwrap(),
        MatchStatus::NoMatch
    );
    assert_eq!(
        match_guard(b"abc--abc", &e, Limits::default()).unwrap(),
        MatchStatus::Ambiguous
    );
}

#[test]
fn empty_pattern_uses_boundaries_and_anchors() {
    let mut e = g("", "", "x", "");
    assert_eq!(
        match_guard("é".as_bytes(), &e, Limits::default()).unwrap(),
        MatchStatus::Ambiguous
    ); // two, not three byte offsets
    e.start_anchor = true;
    assert!(matches!(
        match_guard("é".as_bytes(), &e, Limits::default()).unwrap(),
        MatchStatus::Unique(m) if m.write == (0..0)
    ));
    e.end_anchor = true;
    assert_eq!(
        match_guard(b"x", &e, Limits::default()).unwrap(),
        MatchStatus::NoMatch
    );
}

#[test]
fn applies_independent_edits_in_source_order_independently_of_input_order() {
    let a = g("", "one", "1", " ");
    let b = g(" ", "three", "3", "");
    let mut aa = a.clone();
    aa.start_anchor = true;
    let mut bb = b.clone();
    bb.end_anchor = true;
    let first = apply(
        b"one two three",
        &[aa.clone(), bb.clone()],
        Limits::default(),
    )
    .unwrap();
    let second = apply(b"one two three", &[bb, aa], Limits::default()).unwrap();
    assert_eq!(first.content, b"1 two 3");
    assert_eq!(first.content, second.content);
    assert!(
        first
            .outcomes
            .iter()
            .all(|x| matches!(x, EditOutcome::Applied { .. }))
    );
}

#[test]
fn partial_output_combines_growth_deletion_and_preserved_gaps() {
    let edits = [g("", "a", "longer", " "), g(" ", "z", "", " ")];
    let target = b"a middle z trailing";
    let result = apply(target, &edits, Limits::default()).unwrap();
    assert_eq!(result.content, b"longer middle  trailing");
    let reversed = apply(
        target,
        &[edits[1].clone(), edits[0].clone()],
        Limits::default(),
    )
    .unwrap();
    assert_eq!(result.content, reversed.content);
}

#[test]
fn interference_is_symmetric_but_shared_reads_are_not() {
    let a = g("", "a", "A", "bc");
    let b = g("ab", "c", "C", "");
    let r = apply(b"abc", &[a, b], Limits::default()).unwrap();
    assert_eq!(r.content, b"abc");
    assert_eq!(r.outcomes[0], EditOutcome::Interference { with: vec![1] });
    assert_eq!(r.outcomes[1], EditOutcome::Interference { with: vec![0] });

    let left = g("", "a", "A", "b");
    let right = g("a", "b", "B", "");
    let r = apply(b"ab", &[left, right], Limits::default()).unwrap();
    assert!(
        r.outcomes
            .iter()
            .all(|x| matches!(x, EditOutcome::Interference { .. }))
    );
}

#[test]
fn same_boundary_insertions_conflict() {
    let e1 = g("a", "", "x", "b");
    let e2 = g("a", "", "y", "b");
    let r = apply(b"ab", &[e1, e2], Limits::default()).unwrap();
    assert_eq!(r.content, b"ab");
    assert!(
        r.outcomes
            .iter()
            .all(|x| matches!(x, EditOutcome::Interference { .. }))
    );
}

#[test]
fn failures_do_not_prevent_partial_result() {
    let mut good = g("", "a", "A", "");
    good.start_anchor = true;
    let bad = g("", "z", "Z", "");
    let r = apply(b"abc", &[bad, good], Limits::default()).unwrap();
    assert_eq!(r.content, b"Abc");
    assert_eq!(r.outcomes[0], EditOutcome::NoMatch);
}

#[test]
fn capture_requires_canonical_boundary_and_adds_two_lines() {
    let text = b"zero\none\ntarget\ntwo\nthree\nfour\n";
    let start = text
        .windows(6)
        .position(|window| window == b"target")
        .unwrap();
    let e = capture(
        text,
        CaptureRequest {
            replacement: start..start + 6,
            wider_guard: None,
        },
        b"new\nvalue",
        Limits::default(),
    )
    .unwrap();
    assert_eq!(e.expected, b"target");
    assert_eq!(e.replacement, b"new\nvalue");
    assert_eq!(e.before, b"zero\none\n");
    assert_eq!(e.after, b"\ntwo\n");
    assert!(e.start_anchor);
    assert!(!e.end_anchor);

    assert_eq!(
        capture(
            b"a\r\nb",
            CaptureRequest {
                replacement: 0..1,
                wider_guard: None,
            },
            b"x",
            Limits::default(),
        )
        .unwrap_err(),
        Error::NonCanonicalLineEnding
    );
    assert_eq!(
        capture(
            b"a\nb",
            CaptureRequest {
                replacement: 0..1,
                wider_guard: None,
            },
            b"x\r\ny",
            Limits::default(),
        )
        .unwrap_err(),
        Error::NonCanonicalLineEnding
    );
}

#[test]
fn explicit_wider_guard_unions_with_default_instead_of_receiving_extra_lines() {
    let text = b"a\nb\nc\nd\ne\n";
    let e = capture(
        text,
        CaptureRequest {
            replacement: 4..5,
            wider_guard: Some(0..8),
        },
        b"C",
        Limits::default(),
    )
    .unwrap();
    assert_eq!(e.before, b"a\nb\n");
    assert_eq!(e.after, b"\nd\n");
    assert!(e.start_anchor);
    assert!(!e.end_anchor);
}

#[test]
fn validates_encoding_ranges_and_budgets() {
    assert_eq!(
        match_guard(b"a\r\n", &g("", "a", "", ""), Limits::default()).unwrap_err(),
        Error::NonCanonicalLineEnding
    );
    assert!(matches!(
        capture(
            "é".as_bytes(),
            CaptureRequest {
                replacement: 1..2,
                wider_guard: None,
            },
            b"x",
            Limits::default()
        ),
        Err(Error::NotUtf8Boundary { .. })
    ));
    let tiny = Limits {
        max_input_bytes: 100,
        max_edits: 1,
        max_work: 1,
    };
    assert!(matches!(
        match_guard(b"aaaa", &g("", "a", "", ""), tiny),
        Err(Error::WorkBudgetExceeded { .. })
    ));
    assert!(matches!(
        apply(b"a", &[g("", "a", "", ""), g("", "a", "", "")], tiny),
        Err(Error::TooManyEdits { .. })
    ));

    // Initial uniqueness fits, but validating the final two-line guard does not.
    let final_guard_limit = Limits {
        max_input_bytes: 100,
        max_edits: 1,
        max_work: 3,
    };
    assert!(matches!(
        capture(
            b"abc",
            CaptureRequest {
                replacement: 1..2,
                wider_guard: None,
            },
            b"x",
            final_guard_limit,
        ),
        Err(Error::WorkBudgetExceeded { .. })
    ));
}

#[test]
fn matcher_charges_only_byte_comparisons_performed() {
    let edit = g("", "z", "", "");
    let exact = Limits {
        max_input_bytes: 100,
        max_edits: 1,
        max_work: 3,
    };
    assert_eq!(
        match_guard(b"abc", &edit, exact).unwrap(),
        MatchStatus::NoMatch
    );
    assert_eq!(
        match_guard(
            b"abc",
            &edit,
            Limits {
                max_work: 2,
                ..exact
            }
        )
        .unwrap_err(),
        Error::WorkBudgetExceeded { limit: 2 }
    );
}

#[test]
fn repeated_near_matches_still_exhaust_the_work_budget() {
    let edit = g("", "aaaab", "", "");
    let limits = Limits {
        max_input_bytes: 100,
        max_edits: 1,
        max_work: 20,
    };
    assert_eq!(
        match_guard(&[b'a'; 100], &edit, limits).unwrap_err(),
        Error::WorkBudgetExceeded { limit: 20 }
    );
}

#[test]
fn matcher_skips_impossible_alignments_without_allocating_a_pattern() {
    let target = [b'a'; 10_000];
    let edit = g("0123456789", "needle", "replacement", "abcdefghij");
    let limits = Limits {
        max_input_bytes: target.len(),
        max_edits: 1,
        max_work: 1_500,
    };
    assert_eq!(
        match_guard(&target, &edit, limits).unwrap(),
        MatchStatus::NoMatch
    );
}

#[test]
fn matcher_finds_fragmented_patterns_after_skipped_alignments() {
    let edit = g("before-", "expected", "replacement", "-after");
    let target = b"unrelated bytes before-expected-after unrelated bytes";
    let matched = match_guard(target, &edit, Limits::default()).unwrap();
    assert_eq!(
        matched,
        MatchStatus::Unique(MatchedGuard {
            read: 16..37,
            write: 23..31,
        })
    );
}

#[test]
fn matcher_batches_budget_accounting_without_changing_exact_comparison_count() {
    let edit = g("abc", "def", "replacement", "ghi");
    let target = b"abcdefghi";
    let exact = Limits {
        max_input_bytes: 32,
        max_edits: 1,
        max_work: 9,
    };
    assert!(matches!(
        match_guard(target, &edit, exact).unwrap(),
        MatchStatus::Unique(_)
    ));
    assert_eq!(
        match_guard(
            target,
            &edit,
            Limits {
                max_work: 8,
                ..exact
            }
        )
        .unwrap_err(),
        Error::WorkBudgetExceeded { limit: 8 }
    );
}

#[test]
fn capture_large_mostly_unique_text_uses_actual_comparison_work() {
    let mut source = String::new();
    for line in 0..2_000 {
        source.push_str(&format!("{line:04x}-unique-machine-instruction\n"));
    }
    let needle = "03e8-unique-machine-instruction";
    let start = source.find(needle).unwrap();
    let end = start + needle.len();
    let limits = Limits {
        max_input_bytes: 64 * 1024,
        max_edits: 1,
        // The old candidates * full-pattern charge rejects the final expanded
        // guard (>1 MiB); actual early rejections stay comfortably below this.
        max_work: 200 * 1024,
    };
    let captured = capture(
        source.as_bytes(),
        CaptureRequest {
            replacement: start..end,
            wider_guard: None,
        },
        b"changed-machine-instruction",
        limits,
    )
    .unwrap();
    assert!(matches!(
        match_guard(source.as_bytes(), &captured, limits).unwrap(),
        MatchStatus::Unique(_)
    ));
}

#[test]
fn apply_charges_aggregate_matching_and_interference_work() {
    let mut edit = g("", "a", "A", "");
    edit.start_anchor = true;
    edit.end_anchor = true;
    let limits = Limits {
        max_input_bytes: 100,
        max_edits: 3,
        max_work: 5,
    };
    assert_eq!(
        apply(b"a", &[edit.clone(), edit.clone(), edit], limits).unwrap_err(),
        Error::WorkBudgetExceeded { limit: 5 }
    );
}
