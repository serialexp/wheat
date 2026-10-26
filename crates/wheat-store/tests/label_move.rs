//! Whether a label may move from one view to another: forward when integrating the
//! old view into the new one would change nothing.

mod support;

use std::sync::atomic::Ordering;

use support::*;
use wheat_core::*;
use wheat_store::*;

async fn view(fixture: &Fixture, id: ViewId) -> ViewRecord {
    fixture.reader().view(id).await.unwrap()
}

async fn forward(fixture: &Fixture, from: ViewId, to: ViewId) -> Option<NotForward> {
    let mut budget = MergeBudget::STANDARD;
    fixture
        .reader()
        .forward(
            &view(fixture, from).await,
            &view(fixture, to).await,
            &mut budget,
        )
        .await
        .unwrap()
}

fn diverged(change: ChangeId, edits_added: usize, edits_dropped: usize) -> Option<NotForward> {
    Some(NotForward::Diverged {
        change,
        edits_added,
        edits_dropped,
    })
}

#[tokio::test]
async fn a_view_is_forward_from_itself() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (_, s) = fixture.create(base, base, "s", b"s").await;

    assert_eq!(forward(&fixture, s, s).await, None);
}

#[tokio::test]
async fn a_descendant_is_forward_and_its_ancestor_is_not() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let m = revise(&fixture, base, s, c, Revision::Add("e"), 100).await;

    assert_eq!(forward(&fixture, s, m).await, None);
    // Going back would drop `e`, which the merge keeps.
    assert_eq!(forward(&fixture, m, s).await, diverged(c, 1, 0));
}

#[tokio::test]
async fn a_descendant_that_removed_an_edit_is_forward() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let m = revise(&fixture, base, s, c, Revision::Add("e"), 100).await;
    let x = revise(&fixture, base, m, c, Revision::Remove("e"), 200).await;

    assert_eq!(forward(&fixture, m, x).await, None);
}

#[tokio::test]
async fn a_sibling_lacking_an_addition_is_not_forward() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let m = revise(&fixture, base, s, c, Revision::Add("m"), 100).await;
    let x = revise(&fixture, base, m, c, Revision::Add("x"), 200).await;
    let y = revise(&fixture, base, m, c, Revision::Add("y"), 300).await;

    assert_eq!(forward(&fixture, x, y).await, diverged(c, 1, 0));
}

#[tokio::test]
async fn a_sibling_keeping_an_edit_the_other_removed_is_not_forward() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let m = revise(&fixture, base, s, c, Revision::Add("e"), 100).await;
    let x = revise(&fixture, base, m, c, Revision::Remove("e"), 200).await;
    let y = revise(&fixture, base, m, c, Revision::Add("y"), 300).await;

    // The merge drops `e`, which `y` holds.
    assert_eq!(forward(&fixture, x, y).await, diverged(c, 0, 1));
}

/// O and R made the same edit on their own lines (decision 6), and N revises R,
/// removing it (decision 12): both are forward from O.
#[tokio::test]
async fn the_same_edits_on_another_line_are_forward_and_so_is_removing_them_there() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let o = revise(&fixture, base, s, c, Revision::Add("e"), 100).await;
    let r = revise(&fixture, base, s, c, Revision::Add("e"), 200).await;
    assert_ne!(
        revision_in(&fixture, o, c).await.id(),
        revision_in(&fixture, r, c).await.id()
    );
    let n = revise(&fixture, base, r, c, Revision::Remove("e"), 300).await;

    assert_eq!(forward(&fixture, o, r).await, None);
    assert_eq!(forward(&fixture, o, n).await, None);
}

#[tokio::test]
async fn a_combined_revision_is_forward_from_each_input() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let a = revise(&fixture, base, s, c, Revision::Add("a"), 100).await;
    let b = revise(&fixture, base, s, c, Revision::Add("b"), 200).await;
    let joined = fixture
        .integrate(base, &[a, b])
        .await
        .unwrap()
        .resulting_view;
    assert!(!revision_in(&fixture, joined, c).await.combines.is_empty());

    assert_eq!(forward(&fixture, a, joined).await, None);
    assert_eq!(forward(&fixture, b, joined).await, None);
}

#[tokio::test]
async fn a_view_lacking_a_change_is_not_forward_and_one_with_more_is() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (first, one) = fixture.create(base, base, "a", b"a").await;
    let (_, other) = fixture.create(base, base, "b", b"b").await;
    let (_, more) = fixture.create(base, one, "c", b"c").await;

    assert_eq!(
        forward(&fixture, one, other).await,
        Some(NotForward::Missing { change: first })
    );
    assert_eq!(forward(&fixture, one, more).await, None);
}

/// Checks of one command share a budget: each takes its work off what is left, and a
/// check over what is left is refused naming the change.
#[tokio::test]
async fn checks_share_one_merge_budget() {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (c, s) = fixture.create(base, base, "s", b"s").await;
    let m = revise(&fixture, base, s, c, Revision::Add("m"), 100).await;
    let x = revise(&fixture, base, m, c, Revision::Add("x"), 200).await;
    let (from, to) = (view(&fixture, m).await, view(&fixture, x).await);
    let reader = fixture.reader();

    let mut budget = MergeBudget::STANDARD;
    assert_eq!(reader.forward(&from, &to, &mut budget).await.unwrap(), None);
    let spent = MergeBudget::STANDARD.max_walk_steps - budget.max_walk_steps;
    assert!(spent > 0);
    assert_eq!(reader.forward(&from, &to, &mut budget).await.unwrap(), None);
    assert_eq!(
        MergeBudget::STANDARD.max_walk_steps - budget.max_walk_steps,
        2 * spent
    );

    let mut short = MergeBudget {
        max_walk_steps: spent - 1,
        ..MergeBudget::STANDARD
    };
    match reader.forward(&from, &to, &mut short).await {
        Err(RepositoryPublicationError::MergeOverBudget { change, limit }) => {
            assert_eq!((change, limit), (c, "walk steps"));
        }
        other => panic!("expected over budget, got {other:?}"),
    }
}

/// Reads of checking a move that revised one change once, after `line` revisions of
/// that change and `others` other changes.
async fn reads_of_one_revision_move(line: usize, others: usize) -> usize {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let mut tip = base;
    for index in 0..others {
        let path = format!("other{index}");
        tip = fixture.create(base, tip, &path, path.as_bytes()).await.1;
    }
    let (c, mut tip) = fixture.create(base, tip, "s", b"s").await;
    for index in 0..line {
        let at = i64::try_from(index).unwrap();
        let step = if index % 2 == 0 {
            Revision::Add("t")
        } else {
            Revision::Remove("t")
        };
        tip = revise(&fixture, base, tip, c, step, at).await;
    }
    let moved = revise(&fixture, base, tip, c, Revision::Add("u"), 10_000).await;
    let (from, to) = (view(&fixture, tip).await, view(&fixture, moved).await);
    fixture.objects.reads.store(0, Ordering::Relaxed);
    let mut budget = MergeBudget::STANDARD;
    assert_eq!(
        fixture
            .reader()
            .forward(&from, &to, &mut budget)
            .await
            .unwrap(),
        None
    );
    fixture.objects.reads.load(Ordering::Relaxed)
}

#[tokio::test]
async fn checking_a_move_reads_the_same_however_long_the_change_s_history() {
    let reads = [
        reads_of_one_revision_move(10, 0).await,
        reads_of_one_revision_move(100, 0).await,
    ];
    assert_eq!(reads[0], reads[1], "{reads:?}");
}

#[tokio::test]
async fn checking_a_move_reads_what_differs_not_the_other_changes() {
    let reads = [
        reads_of_one_revision_move(1, 16).await,
        reads_of_one_revision_move(1, 160).await,
    ];
    assert_eq!(reads[0], reads[1], "{reads:?}");
}
/// Sets the label `name` to `label` in a publication of its own.
async fn set_label(fixture: &Fixture, name: &str, label: ViewLabel) {
    let reader = fixture.reader();
    let envelope = reader.envelope(fixture.head().envelope).await.unwrap();
    let mut labels = reader
        .view_label_manifest(envelope.labels)
        .await
        .unwrap()
        .labels;
    labels.insert(name.to_owned(), label);
    let manifest = ViewLabelManifestRecord::new(labels).unwrap();
    let publisher = RepositoryPublisher::new(&fixture.objects, &fixture.content, &fixture.heads);
    let root = publisher
        .stage_envelope(envelope.state, &manifest)
        .await
        .unwrap();
    publisher
        .publish_envelope(fixture.head(), root)
        .await
        .unwrap();
}

async fn label(fixture: &Fixture, name: &str) -> Option<ViewLabel> {
    let reader = fixture.reader();
    let envelope = reader.envelope(fixture.head().envelope).await.unwrap();
    reader
        .view_label_manifest(envelope.labels)
        .await
        .unwrap()
        .labels
        .remove(name)
}

fn annotated(view: ViewId) -> ViewLabel {
    ViewLabel {
        view,
        original_git_ref: Some("refs/tags/v1".to_owned()),
        annotation: Some(LabelAnnotation {
            tagger: Actor {
                name: "Ada".to_owned(),
                email: "ada@example.com".to_owned(),
            },
            tagged_at: Timestamp::default(),
            message: "first".to_owned(),
        }),
    }
}

/// C and D grew apart from L, and the annotated label `v1` names C. Returns the
/// fixture, C and D.
async fn joined_beside_a_label() -> (Fixture, ViewId, ViewId) {
    let fixture = Fixture::new().await;
    let base = fixture.base;
    let (_, l) = fixture.create(base, base, "l", b"l").await;
    let (_, d) = fixture.create(base, l, "y", b"y").await;
    let (_, c) = fixture.create(base, l, "x", b"x").await;
    set_label(&fixture, "v1", annotated(c)).await;
    (fixture, c, d)
}

#[tokio::test]
async fn a_mutation_moves_a_label_in_its_own_publication() {
    let (fixture, c, d) = joined_beside_a_label().await;
    let before = fixture.head();
    let (prepared, outcome) = fixture.mutator().integrate(before, c, &[d]).await.unwrap();
    let joined = outcome.resulting_view;
    assert_eq!(forward(&fixture, c, joined).await, None);

    let after = fixture
        .mutator()
        .publish_moving_label(
            prepared,
            Some(&LabelMove {
                name: "v1".to_owned(),
                from: c,
                to: joined,
            }),
        )
        .await
        .unwrap();

    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(label(&fixture, "v1").await, Some(annotated(joined)));
    fixture.validate().await;
}

#[tokio::test]
async fn a_two_phase_publication_moves_a_label_with_the_mutation() {
    let (fixture, c, d) = joined_beside_a_label().await;
    let before = fixture.head();
    let (prepared, outcome) = fixture.mutator().integrate(before, c, &[d]).await.unwrap();
    let joined = outcome.resulting_view;

    let publication = prepare_mutation_publication_moving_label(
        &fixture.objects,
        &fixture.content,
        &fixture.heads,
        before,
        prepared,
        Some(&LabelMove {
            name: "v1".to_owned(),
            from: c,
            to: joined,
        }),
    )
    .await
    .unwrap();
    assert_eq!(label(&fixture, "v1").await, Some(annotated(c)));
    finalize_mutation_publication(&fixture.heads, publication).unwrap();

    assert_eq!(label(&fixture, "v1").await, Some(annotated(joined)));
    fixture.validate().await;
}

/// A label that no longer names the view the move was checked from, or does not exist,
/// or a target the mutation does not retain, refuses the publication as a whole.
#[tokio::test]
async fn a_label_move_that_no_longer_holds_refuses_the_publication() {
    let (fixture, c, d) = joined_beside_a_label().await;
    set_label(&fixture, "v1", annotated(d)).await;
    let before = fixture.head();
    let unretained = ViewId::from_digest([7; 32]);
    for (name, to, refused) in [
        ("v1", None, Some(Some(d))),
        ("v2", None, Some(None)),
        ("v1", Some(unretained), None),
    ] {
        let (prepared, outcome) = fixture.mutator().integrate(before, c, &[d]).await.unwrap();
        let from = if refused.is_some() { c } else { d };
        let result = fixture
            .mutator()
            .publish_moving_label(
                prepared,
                Some(&LabelMove {
                    name: name.to_owned(),
                    from,
                    to: to.unwrap_or(outcome.resulting_view),
                }),
            )
            .await;
        match (result, refused) {
            (
                Err(RepositoryMutationError::LabelMoved {
                    name: refused_name,
                    expected,
                    actual,
                }),
                Some(named),
            ) => assert_eq!((refused_name.as_str(), expected, actual), (name, c, named)),
            (
                Err(RepositoryMutationError::Publication(
                    RepositoryPublicationError::InvalidReferences(_),
                )),
                None,
            ) => {}
            (other, _) => panic!("{name}: expected a refusal, got {other:?}"),
        }
        assert_eq!(fixture.head(), before);
    }
    assert_eq!(label(&fixture, "v1").await, Some(annotated(d)));
}
