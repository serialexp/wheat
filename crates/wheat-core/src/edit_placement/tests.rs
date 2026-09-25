use super::*;

fn edit(n: u8) -> EditId {
    EditId::from_digest([n; 32])
}

fn later(change: ChangeId, n: u8) -> LaterEdit {
    LaterEdit {
        change,
        edit: edit(n),
    }
}

/// A later change's step; each write is `(edit, start, end, replacement)`.
fn step(change: ChangeId, writes: &[(u8, usize, usize, &str)]) -> LaterStep {
    LaterStep {
        change,
        writes: writes
            .iter()
            .map(|(n, start, end, replacement)| LaterWrite::Range {
                edit: edit(*n),
                write: *start..*end,
                replacement_len: replacement.len(),
            })
            .collect(),
    }
}

/// Applies the writes of `step` the way the step was recorded.
fn apply(bytes: &str, writes: &[(u8, usize, usize, &str)]) -> String {
    let mut sorted = writes.to_vec();
    sorted.sort_by_key(|(_, start, end, _)| (*start, *end));
    let mut out = String::new();
    let mut at = 0;
    for (_, start, end, replacement) in sorted {
        out.push_str(&bytes[at..start]);
        out.push_str(replacement);
        at = end;
    }
    out.push_str(&bytes[at..]);
    out
}

/// The earlier change's own bytes at `range`, from `offset` in its result.
fn below(range: Range<usize>, offset: usize) -> Segment {
    Segment {
        below: offset..offset + range.len(),
        range,
        origin: Origin::Below,
    }
}

/// `edit`'s bytes at `range`, standing for `replaced` of the earlier change's result.
fn above(range: Range<usize>, replaced: Range<usize>, edit: LaterEdit) -> Segment {
    Segment {
        range,
        below: replaced,
        origin: Origin::Above(edit),
    }
}

fn place_text(
    below_text: &str,
    current: &str,
    workspace: &str,
    provenance: &Provenance,
    also_move: &BTreeSet<LaterEdit>,
) -> Placement {
    place(PlacementInput {
        below: Some(below_text.as_bytes()),
        current: Some(current.as_bytes()),
        workspace: Some(workspace.as_bytes()),
        kind_changed: false,
        provenance,
        also_move,
    })
    .unwrap()
}

fn text(placement: &Placement) -> &str {
    std::str::from_utf8(placement.below.as_deref().unwrap()).unwrap()
}

const R: &str = "a\nb\nc\nd\ne\n";

/// C replaces line 3 (`c`) with two lines.
fn replaced_line_three() -> (ChangeId, String, Provenance) {
    let c = ChangeId::new();
    let writes = [(1, 4, 6, "C1\nC2\n")];
    let current = apply(R, &writes);
    let provenance = attribute(Some(R.len()), &[step(c, &writes)]).unwrap();
    (c, current, provenance)
}

#[test]
fn attribute_splits_the_file_around_a_later_block() {
    let (c, current, provenance) = replaced_line_three();
    assert_eq!(current, "a\nb\nC1\nC2\nd\ne\n");
    assert_eq!(
        provenance.segments.unwrap(),
        vec![
            below(0..4, 0),
            above(4..10, 4..6, later(c, 1)),
            below(10..14, 6)
        ]
    );
    assert_eq!(provenance.edits, vec![later(c, 1)]);
    assert!(provenance.whole_file.is_empty());
}

#[test]
fn attribute_follows_stacked_blocks() {
    let base = "0123456789";
    let (c, d) = (ChangeId::new(), ChangeId::new());
    let c_writes = [(1, 2, 5, "abcd")];
    let x1 = apply(base, &c_writes);
    let d_writes = [(2, 3, 8, "Z")];
    let x2 = apply(&x1, &d_writes);
    assert_eq!(x2, "01aZ789");
    let provenance =
        attribute(Some(base.len()), &[step(c, &c_writes), step(d, &d_writes)]).unwrap();
    assert_eq!(
        provenance.segments.unwrap(),
        vec![
            // C's piece keeps the bytes C replaced; D replaced the bytes of what it overwrote.
            below(0..2, 0),
            above(2..3, 2..5, later(c, 1)),
            above(3..4, 5..7, later(d, 2)),
            below(4..7, 7),
        ]
    );
    assert_eq!(provenance.edits, vec![later(c, 1), later(d, 2)]);
}

#[test]
fn attribute_keeps_deletion_markers_on_edges_and_drops_them_inside_writes() {
    let base = "0123456789";
    let (c, d) = (ChangeId::new(), ChangeId::new());
    let c_writes = [(1, 5, 6, "")];
    let x1 = apply(base, &c_writes);
    assert_eq!(x1, "012346789");

    // D overwrites the byte right after C's deletion: the marker stays on D's edge.
    let edge = [(2, 5, 6, "Q")];
    let provenance = attribute(Some(base.len()), &[step(c, &c_writes), step(d, &edge)]).unwrap();
    assert_eq!(
        provenance.segments.unwrap(),
        vec![
            below(0..5, 0),
            above(5..5, 5..6, later(c, 1)),
            above(5..6, 6..7, later(d, 2)),
            below(6..9, 7),
        ]
    );

    // D overwrites across C's deletion: nothing of C is left in the file.
    let across = [(2, 4, 7, "XY")];
    assert_eq!(apply(&x1, &across), "0123XY89");
    let provenance = attribute(Some(base.len()), &[step(c, &c_writes), step(d, &across)]).unwrap();
    assert_eq!(
        provenance.segments.unwrap(),
        vec![
            below(0..4, 0),
            above(4..6, 4..8, later(d, 2)),
            below(6..8, 8)
        ]
    );
    assert_eq!(provenance.edits, vec![later(c, 1), later(d, 2)]);
}

#[test]
fn attribute_handles_several_writes_in_one_step() {
    let base = "0123456789";
    let c = ChangeId::new();
    let writes = [(2, 6, 8, "xyz"), (1, 1, 2, "")];
    assert_eq!(apply(base, &writes), "02345xyz89");
    let provenance = attribute(Some(base.len()), &[step(c, &writes)]).unwrap();
    assert_eq!(
        provenance.segments.unwrap(),
        vec![
            below(0..1, 0),
            above(1..1, 1..2, later(c, 1)),
            below(1..5, 2),
            above(5..8, 6..8, later(c, 2)),
            below(8..10, 8),
        ]
    );
}

#[test]
fn attribute_records_whole_file_edits() {
    let (c, d) = (ChangeId::new(), ChangeId::new());
    let created = LaterStep {
        change: c,
        writes: vec![LaterWrite::WholeFile {
            edit: edit(1),
            len: Some(4),
        }],
    };
    let provenance = attribute(None, std::slice::from_ref(&created)).unwrap();
    assert_eq!(
        provenance.segments.unwrap(),
        vec![above(0..4, 0..0, later(c, 1))]
    );
    assert_eq!(provenance.whole_file, vec![later(c, 1)]);

    let deleted = LaterStep {
        change: d,
        writes: vec![LaterWrite::WholeFile {
            edit: edit(2),
            len: None,
        }],
    };
    let provenance = attribute(None, &[created, deleted]).unwrap();
    assert_eq!(provenance.segments, None);
    assert_eq!(provenance.edits, vec![later(c, 1), later(d, 2)]);
}

#[test]
fn attribute_rejects_evidence_that_does_not_fit() {
    let c = ChangeId::new();
    assert_eq!(
        attribute(Some(3), &[step(c, &[(1, 2, 9, "x")])]),
        Err(PlacementError::InconsistentEvidence)
    );
    assert_eq!(
        attribute(Some(9), &[step(c, &[(1, 0, 4, "x"), (2, 2, 6, "y")])]),
        Err(PlacementError::InconsistentEvidence)
    );
    assert_eq!(
        attribute(None, &[step(c, &[(1, 0, 0, "x")])]),
        Err(PlacementError::InconsistentEvidence)
    );
}

#[test]
fn an_edit_on_the_earlier_changes_bytes_goes_straight_in() {
    let (_, current, provenance) = replaced_line_three();
    let workspace = current.replace("a\n", "A\n");
    let placement = place_text(R, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "A\nb\nc\nd\ne\n");
    assert!(placement.moved.is_empty());
    assert!(placement.notices.is_empty());
}

#[test]
fn an_edit_inside_a_later_block_moves_the_whole_block() {
    let (c, current, provenance) = replaced_line_three();
    let workspace = current.replace("C2\n", "X2\n");
    let placement = place_text(R, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "a\nb\nC1\nX2\nd\ne\n");
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
    assert_eq!(
        placement.notices,
        vec![MoveNotice::Block {
            edit: later(c, 1),
            yours: Lines { first: 4, last: 4 },
            theirs: Lines { first: 3, last: 4 },
        }]
    );
}

#[test]
fn a_line_inserted_next_to_a_later_block_does_not_move_it() {
    let (_, current, provenance) = replaced_line_three();
    let workspace = current.replace("C2\n", "C2\nn\n");
    let placement = place_text(R, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "a\nb\nc\nn\nd\ne\n");
    assert!(placement.moved.is_empty());

    let workspace = current.replace("b\n", "b\nn\n");
    let placement = place_text(R, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "a\nb\nn\nc\nd\ne\n");
    assert!(placement.moved.is_empty());
}

#[test]
fn an_edit_across_a_later_deletion_moves_the_deletion() {
    let c = ChangeId::new();
    let writes = [(1, 4, 6, "")];
    let current = apply(R, &writes);
    assert_eq!(current, "a\nb\nd\ne\n");
    let provenance = attribute(Some(R.len()), &[step(c, &writes)]).unwrap();

    // Next to the deletion: C keeps deleting `c`.
    let placement = place_text(R, &current, "a\nB\nd\ne\n", &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "a\nB\nc\nd\ne\n");
    assert!(placement.moved.is_empty());

    // Across it: the author's version replaces the lines around it, and C gives it up.
    let placement = place_text(R, &current, "a\nx\ne\n", &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "a\nx\ne\n");
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
}

#[test]
fn a_moved_block_takes_the_place_of_the_bytes_it_replaced() {
    // C replaced the whole single-line file.
    let c = ChangeId::new();
    let base = "ancestor one\n";
    let writes = [(1, 0, base.len(), "descendant value\n")];
    let current = apply(base, &writes);
    let provenance = attribute(Some(base.len()), &[step(c, &writes)]).unwrap();
    let workspace = "ancestor revised\n";
    let placement = place_text(base, &current, workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), workspace);
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));

    // C and D each replaced a line and nothing of the earlier file is left. Editing C's line
    // moves only C's block; D's line stays for D to replace.
    let d = ChangeId::new();
    let c_writes = [(1, 0, 2, "x\n")];
    let x1 = apply("a\nb\n", &c_writes);
    let d_writes = [(2, 2, 4, "y\n")];
    let current = apply(&x1, &d_writes);
    let provenance = attribute(Some(4), &[step(c, &c_writes), step(d, &d_writes)]).unwrap();
    let placement = place_text("a\nb\n", &current, "z\ny\n", &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), "z\nb\n");
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
}

#[test]
fn a_line_added_after_a_later_block_at_the_end_goes_after_the_bytes_it_replaced() {
    let c = ChangeId::new();
    let writes = [(1, 2, 4, "x\n")];
    let current = apply("a\nb\n", &writes);
    let provenance = attribute(Some(4), &[step(c, &writes)]).unwrap();
    let placement = place_text(
        "a\nb\n",
        &current,
        "a\nx\nn\n",
        &provenance,
        &BTreeSet::new(),
    );
    assert_eq!(text(&placement), "a\nb\nn\n");
    assert!(placement.moved.is_empty());
}

#[test]
fn a_moved_deletion_takes_the_deleted_bytes_along() {
    let c = ChangeId::new();
    let writes = [(1, 4, 6, "")];
    let current = apply(R, &writes);
    let provenance = attribute(Some(R.len()), &[step(c, &writes)]).unwrap();
    let workspace = current.replace("a\n", "A\n");
    let placement = place_text(
        R,
        &current,
        &workspace,
        &provenance,
        &BTreeSet::from([later(c, 1)]),
    );
    assert_eq!(text(&placement), "A\nb\nd\ne\n");
}

#[test]
fn moving_a_block_pulls_in_blocks_it_merges_with() {
    // C rewrites lines 2-3 and D rewrites line 4 right after it. The author edits line 3 and
    // 4 together, so both blocks move.
    let (c, d) = (ChangeId::new(), ChangeId::new());
    let c_writes = [(1, 2, 6, "B\nC\n")];
    let x1 = apply(R, &c_writes);
    let d_writes = [(2, 6, 8, "D\n")];
    let current = apply(&x1, &d_writes);
    assert_eq!(current, "a\nB\nC\nD\ne\n");
    let provenance = attribute(Some(R.len()), &[step(c, &c_writes), step(d, &d_writes)]).unwrap();
    let placement = place_text(
        R,
        &current,
        "a\nB\nX\nY\ne\n",
        &provenance,
        &BTreeSet::new(),
    );
    assert_eq!(text(&placement), "a\nB\nX\nY\ne\n");
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1), later(d, 2)]));
}

#[test]
fn also_moved_edits_take_their_blocks_along() {
    let (c, current, provenance) = replaced_line_three();
    let workspace = current.replace("a\n", "A\n");
    let placement = place_text(
        R,
        &current,
        &workspace,
        &provenance,
        &BTreeSet::from([later(c, 1)]),
    );
    assert_eq!(text(&placement), "A\nb\nC1\nC2\nd\ne\n");
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
    assert_eq!(
        placement.notices,
        vec![MoveNotice::NoLongerFits {
            edit: later(c, 1),
            theirs: Lines { first: 3, last: 4 },
        }]
    );
}

#[test]
fn an_untouched_file_keeps_the_earlier_changes_version() {
    let (_, current, provenance) = replaced_line_three();
    let placement = place_text(R, &current, &current, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), R);
    assert!(placement.moved.is_empty());
}

#[test]
fn a_file_without_later_edits_is_the_workspace_file() {
    let placement = place_text(R, R, "new\n", &Provenance::default(), &BTreeSet::new());
    assert_eq!(text(&placement), "new\n");
    assert!(placement.moved.is_empty());
}

#[test]
fn whole_file_moves_name_the_reason_and_every_change() {
    let (c, d) = (ChangeId::new(), ChangeId::new());
    let created = attribute(
        None,
        &[
            LaterStep {
                change: c,
                writes: vec![LaterWrite::WholeFile {
                    edit: edit(1),
                    len: Some(3),
                }],
            },
            step(d, &[(2, 0, 1, "y")]),
        ],
    )
    .unwrap();
    let no_moves = BTreeSet::new();
    let placement = place(PlacementInput {
        below: None,
        current: Some(b"yyz"),
        workspace: Some(b"yyw"),
        kind_changed: false,
        provenance: &created,
        also_move: &no_moves,
    })
    .unwrap();
    assert_eq!(placement.below.as_deref(), Some(b"yyw".as_slice()));
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1), later(d, 2)]));
    assert_eq!(
        placement.notices,
        vec![MoveNotice::WholeFile {
            reason: WholeFileReason::CreatedLater,
            changes: BTreeSet::from([c, d]),
        }]
    );

    let (c, current, provenance) = replaced_line_three();
    let placement = place(PlacementInput {
        below: Some(R.as_bytes()),
        current: Some(current.as_bytes()),
        workspace: None,
        kind_changed: false,
        provenance: &provenance,
        also_move: &no_moves,
    })
    .unwrap();
    assert_eq!(placement.below, None);
    assert_eq!(
        placement.notices,
        vec![MoveNotice::WholeFile {
            reason: WholeFileReason::WorkspaceReplaces,
            changes: BTreeSet::from([c]),
        }]
    );

    let placement = place(PlacementInput {
        below: Some(R.as_bytes()),
        current: Some(current.as_bytes()),
        workspace: Some(current.as_bytes()),
        kind_changed: true,
        provenance: &provenance,
        also_move: &no_moves,
    })
    .unwrap();
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
}

#[test]
fn a_small_edit_in_a_large_file_only_diffs_its_neighbourhood() {
    // Far more lines than one line diff accepts.
    let lines = (0..50_000)
        .map(|n| format!("line {n}\n"))
        .collect::<Vec<_>>();
    let base = lines.concat();
    let c = ChangeId::new();
    let start = lines[..25_000].iter().map(String::len).sum::<usize>();
    let writes = [(1, start, start + lines[25_000].len(), "later\n")];
    let current = apply(&base, &writes);
    let provenance = attribute(Some(base.len()), &[step(c, &writes)]).unwrap();

    let workspace = current.replacen("line 10\n", "line ten\n", 1);
    let placement = place_text(&base, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(
        text(&placement),
        base.replacen("line 10\n", "line ten\n", 1)
    );
    assert!(placement.moved.is_empty());

    let workspace = current.replacen("later\n", "mine\n", 1);
    let placement = place_text(&base, &current, &workspace, &provenance, &BTreeSet::new());
    assert_eq!(text(&placement), workspace);
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
}

#[test]
fn bytes_without_lines_are_placed_as_one_region() {
    let base = b"\0\0\0\0\0\0\0\0".to_vec();
    let c = ChangeId::new();
    let provenance = attribute(Some(base.len()), &[step(c, &[(1, 6, 7, "\x01")])]).unwrap();
    let current = b"\0\0\0\0\0\0\x01\0";
    let workspace = b"\0\x02\0\0\0\0\x01\0";
    let no_moves = BTreeSet::new();
    let placement = place(PlacementInput {
        below: Some(&base),
        current: Some(current),
        workspace: Some(workspace),
        kind_changed: false,
        provenance: &provenance,
        also_move: &no_moves,
    })
    .unwrap();
    // There is no line to narrow the author's edit to, so it covers C's byte as well.
    assert_eq!(placement.below.as_deref(), Some(workspace.as_slice()));
    assert_eq!(placement.moved, BTreeSet::from([later(c, 1)]));
}

#[test]
fn place_rejects_provenance_for_a_different_file() {
    let (_, current, provenance) = replaced_line_three();
    let no_moves = BTreeSet::new();
    let error = place(PlacementInput {
        below: Some(R.as_bytes()),
        current: Some(&current.as_bytes()[1..]),
        workspace: Some(b"x"),
        kind_changed: false,
        provenance: &provenance,
        also_move: &no_moves,
    });
    assert_eq!(error, Err(PlacementError::InconsistentEvidence));

    // The runs must also cover exactly the earlier change's file.
    let error = place(PlacementInput {
        below: Some(&R.as_bytes()[1..]),
        current: Some(current.as_bytes()),
        workspace: Some(b"x"),
        kind_changed: false,
        provenance: &provenance,
        also_move: &no_moves,
    });
    assert_eq!(error, Err(PlacementError::InconsistentEvidence));
}
