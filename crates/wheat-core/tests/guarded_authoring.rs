use std::ops::Range;

use wheat_core::guarded::{
    GuardedEdit, Limits, apply,
    authoring::{AuthoringError, normalize, normalize_with_lineage},
};

fn numbered_lines(count: usize) -> String {
    (0..count)
        .map(|i| format!("unique line {i:02}\n"))
        .collect()
}

fn line_range(text: &str, line: usize) -> Range<usize> {
    let start = text
        .match_indices('\n')
        .nth(line.wrapping_sub(1))
        .map_or(0, |(offset, _)| offset + 1);
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset + 1);
    start..end
}

fn replace_line(text: &str, line: usize, replacement: &str) -> String {
    let range = line_range(text, line);
    format!(
        "{}{}{}",
        &text[..range.start],
        replacement,
        &text[range.end..]
    )
}

fn assert_applies(authored: &str, edits: &[GuardedEdit], expected: &str, limits: Limits) {
    let applied = apply(authored.as_bytes(), edits, limits).unwrap();
    assert_eq!(applied.content, expected.as_bytes());
    assert!(
        applied
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, wheat_core::guarded::EditOutcome::Applied { .. }))
    );
}

#[test]
fn repeated_add_consolidates_against_authored_base_and_undo_removes_edit() {
    let limits = Limits::default();
    let first = normalize(b"foo\n", &[], b"foo\n", b"bar\n", None, limits).unwrap();
    assert_eq!(first.len(), 1);
    let next = normalize(b"foo\n", &first, b"bar\n", b"baz\n", None, limits).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].expected, b"foo\n");
    assert_eq!(next[0].replacement, b"baz\n");
    assert_ne!(first, next);
    assert_eq!(apply(b"foo\n", &next, limits).unwrap().content, b"baz\n");
    assert!(
        normalize(b"foo\n", &next, b"baz\n", b"foo\n", None, limits)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unchanged_add_preserves_definitions_and_wrong_baseline_is_rejected() {
    let limits = Limits::default();
    let first = normalize(b"old\n", &[], b"old\n", b"new\n", None, limits).unwrap();
    assert_eq!(
        normalize(b"old\n", &first, b"new\n", b"new\n", None, limits).unwrap(),
        first
    );
    assert!(matches!(
        normalize(b"old\n", &first, b"wrong\n", b"new\n", None, limits),
        Err(AuthoringError::BaselineMismatch)
    ));
}

#[test]
fn empty_file_insert_and_missing_final_newline_remain_exact() {
    let limits = Limits::default();
    let edits = normalize(b"", &[], b"", "世界".as_bytes(), None, limits).unwrap();
    assert_eq!(
        apply(b"", &edits, limits).unwrap().content,
        "世界".as_bytes()
    );
    assert!(edits[0].start_anchor && edits[0].end_anchor);
}

#[test]
fn distant_add_preserves_existing_definition_after_position_shift() {
    let base: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let baseline = base.replace("line 25\n", "modified 25\n");
    let limits = Limits::default();
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        baseline.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    let workspace = format!("new prefix\n{baseline}");
    let next = normalize(
        base.as_bytes(),
        &first,
        baseline.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert!(next.contains(&first[0]));
    assert_eq!(next.len(), 2);
    assert_eq!(
        apply(base.as_bytes(), &next, limits).unwrap().content,
        workspace.as_bytes()
    );
}

#[test]
fn sixteen_unchanged_definitions_survive_an_independent_add() {
    let base = numbered_lines(500);
    let limits = Limits::default();
    let mut baseline = base.clone();
    let mut existing = Vec::new();
    for n in 0..16 {
        let workspace = replace_line(&baseline, 10 + n * 28, &format!("replacement {n:02}\n"));
        existing = normalize(
            base.as_bytes(),
            &existing,
            baseline.as_bytes(),
            workspace.as_bytes(),
            None,
            limits,
        )
        .unwrap();
        baseline = workspace;
    }
    assert_eq!(existing.len(), 16);
    let workspace = replace_line(&baseline, 490, "independent replacement\n");
    let next = normalize(
        base.as_bytes(),
        &existing,
        baseline.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(next.len(), 17);
    assert!(existing.iter().all(|edit| next.contains(edit)));
    assert_applies(&base, &next, &workspace, limits);
}

#[test]
fn modification_inside_guard_consolidates_against_authored_base() {
    let base = numbered_lines(14);
    let first_result = replace_line(&base, 6, "first replacement\n");
    let limits = Limits::default();
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_result.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(first.len(), 1);

    // Default capture retains two lines on each side, so line 5 is guard material.
    let workspace = replace_line(&first_result, 5, "guard replacement\n");
    let next = normalize(
        base.as_bytes(),
        &first,
        first_result.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(next.len(), 1);
    assert_ne!(next[0], first[0]);
    assert!(
        next[0]
            .expected
            .windows(b"unique line 05\n".len())
            .any(|w| w == b"unique line 05\n")
    );
    assert_applies(&base, &next, &workspace, limits);
}

#[test]
fn shared_unchanged_guard_context_does_not_consolidate() {
    let base = numbered_lines(18);
    let first_result = replace_line(&base, 5, "left replacement\n");
    let limits = Limits::default();
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_result.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    let workspace = replace_line(&first_result, 10, "right replacement\n");
    let next = normalize(
        base.as_bytes(),
        &first,
        first_result.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(next.len(), 2);
    assert!(
        next.contains(&first[0]),
        "unmodified definition lost identity"
    );
    assert_applies(&base, &next, &workspace, limits);
}

#[test]
fn explicit_wider_guard_is_retained_and_guard_change_rebuilds() {
    let base = numbered_lines(20);
    let changed = replace_line(&base, 10, "center replacement\n");
    let limits = Limits::default();
    let guard = line_range(&changed, 3).start..line_range(&changed, 16).end;
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        changed.as_bytes(),
        Some(std::slice::from_ref(&guard)),
        limits,
    )
    .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].before,
        base.as_bytes()[guard.start..line_range(&base, 10).start]
    );
    assert_eq!(
        first[0].after,
        base.as_bytes()[line_range(&base, 10).end..line_range(&base, 16).end]
    );

    let workspace = replace_line(&changed, 3, "wide guard changed\n");
    let next = normalize(
        base.as_bytes(),
        &first,
        changed.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(next.len(), 1);
    assert_ne!(next[0], first[0]);
    assert_applies(&base, &next, &workspace, limits);
}

#[test]
fn boundary_guard_changes_and_reintroduction_have_exact_semantics() {
    let base = numbered_lines(10);
    let first_result = replace_line(&base, 1, "edge replacement\n");
    let limits = Limits::default();
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_result.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert!(first[0].start_anchor);

    let workspace = replace_line(&first_result, 0, "start guard changed\n");
    let consolidated = normalize(
        base.as_bytes(),
        &first,
        first_result.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(consolidated.len(), 1);
    assert_applies(&base, &consolidated, &workspace, limits);

    let removed = normalize(
        base.as_bytes(),
        &consolidated,
        workspace.as_bytes(),
        base.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert!(removed.is_empty());
    let reintroduced = normalize(
        base.as_bytes(),
        &removed,
        base.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(
        reintroduced, consolidated,
        "definition is deterministic; identity/lineage is assigned above core authoring"
    );
}

#[test]
fn lineage_reports_disjoint_components_without_unaffected_members() {
    let base = numbered_lines(30);
    let limits = Limits::default();
    let first_workspace = replace_line(&base, 5, "first old replacement\n");
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    let existing_workspace = replace_line(&first_workspace, 25, "second old replacement\n");
    let existing = normalize(
        base.as_bytes(),
        &first,
        first_workspace.as_bytes(),
        existing_workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(existing.len(), 2);

    let workspace = replace_line(&existing_workspace, 5, "first new replacement\n");
    let normalized = normalize_with_lineage(
        base.as_bytes(),
        &existing,
        existing_workspace.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(normalized.components.len(), 1);
    assert_eq!(normalized.components[0].members, vec![0]);
    assert!(normalized.components[0].replacement.is_some());
    assert_eq!(normalized.edits.len(), 2);
    assert_applies(&base, &normalized.edits, &workspace, limits);
}

#[test]
fn lineage_reports_final_transitive_component_members() {
    let base = numbered_lines(18);
    let limits = Limits::default();
    let first_result = replace_line(&base, 5, "first replacement\n");
    let first_guard = line_range(&first_result, 3).start..line_range(&first_result, 8).end;
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_result.as_bytes(),
        Some(std::slice::from_ref(&first_guard)),
        limits,
    )
    .unwrap();
    let second_result = replace_line(&first_result, 10, "second replacement\n");
    let existing = normalize(
        base.as_bytes(),
        &first,
        first_result.as_bytes(),
        second_result.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    let workspace = replace_line(&second_result, 8, "bridge replacement\n");
    let normalized = normalize_with_lineage(
        base.as_bytes(),
        &existing,
        second_result.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(normalized.components.len(), 1);
    assert_eq!(normalized.components[0].members, vec![0, 1]);
    assert!(normalized.components[0].replacement.is_some());
}

#[test]
fn lineage_marks_net_undo_without_replacement() {
    let limits = Limits::default();
    let first = normalize(b"foo\n", &[], b"foo\n", b"bar\n", None, limits).unwrap();
    let normalized =
        normalize_with_lineage(b"foo\n", &first, b"bar\n", b"foo\n", None, limits).unwrap();
    assert!(normalized.edits.is_empty());
    assert_eq!(normalized.components.len(), 1);
    assert_eq!(normalized.components[0].members, vec![0]);
    assert_eq!(normalized.components[0].replacement, None);
}

#[test]
fn two_new_hunks_consolidate_when_default_guard_reaches_other_write() {
    let base = numbered_lines(16);
    let limits = Limits::default();
    let workspace = replace_line(
        &replace_line(&base, 5, "first new replacement\n"),
        7,
        "second new replacement\n",
    );
    let edits = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(edits.len(), 1);
    assert_applies(&base, &edits, &workspace, limits);
}

#[test]
fn guard_growth_consolidates_transitively_until_fixed_point() {
    let base = numbered_lines(18);
    let limits = Limits::default();
    let first_result = replace_line(&base, 5, "first replacement\n");
    let first_guard = line_range(&first_result, 3).start..line_range(&first_result, 8).end;
    let first = normalize(
        base.as_bytes(),
        &[],
        base.as_bytes(),
        first_result.as_bytes(),
        Some(std::slice::from_ref(&first_guard)),
        limits,
    )
    .unwrap();

    let second_result = replace_line(&first_result, 10, "second replacement\n");
    let existing = normalize(
        base.as_bytes(),
        &first,
        first_result.as_bytes(),
        second_result.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(existing.len(), 2);
    assert!(existing.contains(&first[0]));

    // This modifies E1's explicit guard. Recapturing E1 plus the new write grows
    // the default two-line margin through E2's write, so E2 must also be subsumed.
    let workspace = replace_line(&second_result, 8, "bridge replacement\n");
    let consolidated = normalize(
        base.as_bytes(),
        &existing,
        second_result.as_bytes(),
        workspace.as_bytes(),
        None,
        limits,
    )
    .unwrap();
    assert_eq!(consolidated.len(), 1);
    assert!(!existing.contains(&consolidated[0]));
    assert_applies(&base, &consolidated, &workspace, limits);
}

#[test]
fn independent_edit_and_ambiguous_separator_are_authored_conservatively() {
    let limits = Limits::default();
    let baseline = b"head\nold\nmiddle\n\nend\n";

    // The early replacement has unique adjacent anchors and is safe to capture.
    let early_only = normalize(
        baseline,
        &[],
        baseline,
        b"head\nnew\nmiddle\n\nend\n",
        None,
        limits,
    )
    .unwrap();
    assert_applies(
        "head\nold\nmiddle\n\nend\n",
        &early_only,
        "head\nnew\nmiddle\n\nend\n",
        limits,
    );

    // The extra blank line can be attributed to either adjacent blank-line
    // match. Author the invariant component covering both alternatives rather
    // than treating the renderer's selected blank as provenance.
    let result = normalize(
        baseline,
        &[],
        baseline,
        b"head\nnew\nmiddle\n\n\nend\n",
        None,
        limits,
    )
    .unwrap();
    // The two display hunks share the expanded blank-line component and thus
    // intentionally consolidate; unrelated components would remain separate.
    assert_eq!(result.len(), 1);
    assert_applies(
        "head\nold\nmiddle\n\nend\n",
        &result,
        "head\nnew\nmiddle\n\n\nend\n",
        limits,
    );
}

#[test]
fn ambiguous_repeated_line_deletion_uses_the_complete_component_envelope() {
    let limits = Limits::default();
    let base = b"head\nrepeat\nrepeat\ntail\n";
    let workspace = b"head\nrepeat\ntail\n";
    let result = normalize(base, &[], base, workspace, None, limits).unwrap();
    assert_eq!(result.len(), 1);
    // The repeated lines are part of the expected component, not selected as
    // the provenance of a one-line deletion.
    assert_eq!(result[0].expected, b"repeat\nrepeat\n");
    assert_applies(
        "head\nrepeat\nrepeat\ntail\n",
        &result,
        "head\nrepeat\ntail\n",
        limits,
    );
}

#[test]
fn explicit_guard_validation_rejects_unrelated_and_non_boundary_ranges() {
    let base = "alpha\nbravo\n世界\ndelta\n";
    let workspace = base.replace("bravo\n", "changed\n");
    let limits = Limits::default();
    assert!(matches!(
        normalize(
            base.as_bytes(),
            &[],
            base.as_bytes(),
            workspace.as_bytes(),
            Some(&[line_range(&workspace, 3)]),
            limits,
        ),
        Err(AuthoringError::InvalidExplicitGuard { index: 0 })
    ));
    let inside_multibyte = workspace.find('世').unwrap() + 1;
    let invalid_guard = inside_multibyte..workspace.len();
    assert!(matches!(
        normalize(
            base.as_bytes(),
            &[],
            base.as_bytes(),
            workspace.as_bytes(),
            Some(std::slice::from_ref(&invalid_guard)),
            limits,
        ),
        Err(AuthoringError::InvalidExplicitGuard { index: 0 })
    ));
}
