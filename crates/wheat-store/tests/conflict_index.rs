//! The conflict index: each view's unresolved conflicts in canonical order, checked
//! against a walk of the view's history and its conflict set, and what a page costs.

use crate::support;

use std::{collections::BTreeSet, num::NonZeroUsize, sync::atomic::Ordering};

use support::*;
use wheat_core::*;
use wheat_store::*;

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

/// A change on top of `effective` making `edits`; returns it and the view it makes.
async fn create(fixture: &Fixture, effective: ViewId, edits: RevisionEdits) -> (ChangeId, ViewId) {
    let change = fixture.next_change();
    let (prepared, outcome) = fixture
        .mutator()
        .create_on_frontier(
            fixture.head(),
            fixture.base,
            effective,
            ChangeProposal {
                id: change,
                metadata: ChangeMetadata {
                    title: format!("change {change}"),
                    ..ChangeMetadata::default()
                },
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata::default(),
                edits,
            },
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    (change, outcome.resulting_view)
}

/// A history of `files` changes each adding `f{i}` as `a{i}`, then `files` more each
/// rewriting `f{i}` to `b{i}`. Returns the view and the adding changes.
async fn history(fixture: &Fixture, files: usize) -> (ViewId, Vec<ChangeId>) {
    let mut view = fixture.base;
    let mut adds = Vec::with_capacity(files);
    for file in 0..files {
        let bytes = format!("a{file}").into_bytes();
        fixture.content.put(&bytes).unwrap();
        let (change, next) = create(fixture, view, exact(&format!("f{file}"), None, &bytes)).await;
        adds.push(change);
        view = next;
    }
    for file in 0..files {
        let (found, bytes) = (format!("a{file}"), format!("b{file}"));
        fixture.content.put(bytes.as_bytes()).unwrap();
        let edits = exact(
            &format!("f{file}"),
            Some(found.as_bytes()),
            bytes.as_bytes(),
        );
        view = create(fixture, view, edits).await.1;
    }
    (view, adds)
}

/// Revises the change adding `f{file}` to add `x{file}` instead, so the later change
/// rewriting it conflicts. Returns the view that makes.
async fn break_file(fixture: &Fixture, view: ViewId, change: ChangeId, file: usize) -> ViewId {
    let bytes = format!("x{file}").into_bytes();
    fixture.content.put(&bytes).unwrap();
    let predecessor = revision_in(fixture, view, change).await;
    let mut edits = exact(&format!("f{file}"), None, &bytes);
    edits
        .transition
        .removed
        .extend(predecessor.edits.iter().copied());
    let current = RevisionMapStore::new(&fixture.objects)
        .get(fixture.reader().view(view).await.unwrap().revisions, change)
        .await
        .unwrap()
        .unwrap();
    let (prepared, outcome) = fixture
        .mutator()
        .revise(
            fixture.head(),
            fixture.base,
            view,
            change,
            current,
            RevisionMetadata::default(),
            edits,
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    outcome.resulting_view
}

/// Resolves `conflict` in `view` by keeping the file the edit found.
async fn resolve(fixture: &Fixture, view: ViewId, conflict: &IndexedConflict) -> ViewId {
    let reader = fixture.reader();
    let application = reader.application(conflict.application).await.unwrap();
    let record = reader.conflict(conflict.conflict).await.unwrap();
    let mut resolved = std::collections::BTreeMap::new();
    for (edit, evidence) in &record.edits {
        resolved.insert(reader.edit(*edit).await.unwrap().path, evidence.target);
    }
    let (prepared, outcome) = fixture
        .mutator()
        .resolve_conflict(
            fixture.head(),
            fixture.base,
            view,
            PersistedConflictResolution {
                metadata: RevisionMetadata::default(),
                conflict_revision: application.revision_id,
                application: conflict.application,
                resolved,
            },
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    outcome.resulting_view
}

/// `view`'s unresolved conflicts found by walking its whole history, checked against
/// its conflict set.
async fn walked(fixture: &Fixture, view: ViewId) -> Vec<IndexedConflict> {
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let page = browser.history(view, None, limit(100_000)).await.unwrap();
    let mut conflicts = Vec::new();
    for item in page.items.iter().rev().filter(|item| item.conflicted) {
        let application = fixture
            .reader()
            .application(item.application)
            .await
            .unwrap();
        let ApplicationResultRecord::Conflicted { conflict, .. } = application.result else {
            panic!("a conflicted item's application is clean");
        };
        conflicts.push(IndexedConflict {
            position: item.position,
            conflict,
            application: item.application,
            change: item.change,
        });
    }
    let record = fixture.reader().view(view).await.unwrap();
    let mut cursor = ConflictSetStore::new(&fixture.objects)
        .cursor(record.unresolved_conflicts)
        .await
        .unwrap();
    let mut set = BTreeSet::new();
    while let Some(conflict) = cursor.next().await.unwrap() {
        set.insert(conflict);
    }
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.conflict)
            .collect::<BTreeSet<_>>(),
        set,
        "the walk finds the view's conflict set"
    );
    conflicts
}

/// Every conflict `index` holds for `view`, `size` at a time.
async fn paged(
    fixture: &Fixture,
    index: &ConflictIndex,
    view: ViewId,
    size: usize,
) -> Vec<IndexedConflict> {
    let record = fixture.reader().view(view).await.unwrap();
    let mut conflicts = Vec::new();
    let mut after = None;
    loop {
        let page = index
            .conflicts(&fixture.reader(), &record, after, limit(size))
            .await
            .unwrap();
        assert!(page.conflicts.len() <= size);
        assert!(
            page.next.is_none() || page.conflicts.len() == size,
            "only a full page has a next one"
        );
        conflicts.extend(page.conflicts);
        match page.next {
            Some(next) => after = Some(next),
            None => return conflicts,
        }
    }
}

/// Checks everything `index` says about `view` against the walk.
async fn check(fixture: &Fixture, index: &ConflictIndex, view: ViewId) -> Vec<IndexedConflict> {
    let expected = walked(fixture, view).await;
    let reader = fixture.reader();
    let record = reader.view(view).await.unwrap();
    assert_eq!(
        index.count(&reader, &record).await.unwrap(),
        expected.len() as u64
    );
    for size in [1, 2, 3, 100] {
        assert_eq!(
            paged(fixture, index, view, size).await,
            expected,
            "pages of {size}"
        );
    }
    for position in 0..=record.order.size {
        assert_eq!(
            index.at(&reader, &record, position).await.unwrap(),
            expected
                .iter()
                .find(|conflict| conflict.position == position)
                .copied(),
            "at {position}"
        );
        // A page after a position starts at the first conflict past it.
        let page = index
            .conflicts(&reader, &record, Some(position), limit(1))
            .await
            .unwrap();
        assert_eq!(
            page.conflicts.first(),
            expected
                .iter()
                .find(|conflict| conflict.position > position)
        );
    }
    expected
}

#[tokio::test]
async fn a_view_s_conflicts_are_its_conflicted_applications_in_canonical_order() {
    let fixture = Fixture::new().await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let (clean, adds) = history(&fixture, 12).await;
    assert_eq!(check(&fixture, &index, clean).await, []);

    let mut view = clean;
    for file in [1, 4, 7, 10] {
        view = break_file(&fixture, view, adds[file], file).await;
    }
    let conflicted = check(&fixture, &index, view).await;
    assert_eq!(conflicted.len(), 4);

    // Resolving the second drops it, keeps the others, and leaves the view before
    // the resolution as it was.
    let resolved = resolve(&fixture, view, &conflicted[1]).await;
    let remaining = check(&fixture, &index, resolved).await;
    assert_eq!(
        remaining
            .iter()
            .map(|conflict| conflict.change)
            .collect::<Vec<_>>(),
        [conflicted[0], conflicted[2], conflicted[3]].map(|conflict| conflict.change)
    );
    assert_eq!(check(&fixture, &index, view).await, conflicted);
    // Earlier views on the way are indexed too.
    assert_eq!(check(&fixture, &index, clean).await, []);
}

#[tokio::test]
async fn many_conflicts_page_and_seek_like_the_walk() {
    let fixture = Fixture::new().await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let (mut view, adds) = history(&fixture, 40).await;
    for (file, change) in adds.iter().enumerate() {
        if file % 5 != 2 {
            view = break_file(&fixture, view, *change, file).await;
        }
    }
    assert_eq!(check(&fixture, &index, view).await.len(), 32);
}

#[tokio::test]
async fn catching_up_indexes_only_what_the_index_lacks() {
    let fixture = Fixture::new().await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let reader = fixture.reader();
    let (view, adds) = history(&fixture, 5).await;
    let record = reader.view(view).await.unwrap();
    assert_eq!(index.catch_up(&reader, &record).await.unwrap(), 10);
    assert_eq!(index.catch_up(&reader, &record).await.unwrap(), 0);
    // Revising the second change recomposes it and the eight after it.
    let broken = break_file(&fixture, view, adds[1], 1).await;
    let record = reader.view(broken).await.unwrap();
    assert_eq!(index.catch_up(&reader, &record).await.unwrap(), 9);
    assert_eq!(index.count(&reader, &record).await.unwrap(), 1);
    let base = reader.view(fixture.base).await.unwrap();
    assert_eq!(index.catch_up(&reader, &base).await.unwrap(), 0);
    assert_eq!(index.count(&reader, &base).await.unwrap(), 0);
}

#[tokio::test]
async fn an_unreadable_index_file_is_emptied_and_built_again() {
    let fixture = Fixture::new().await;
    let (view, adds) = history(&fixture, 4).await;
    let view = break_file(&fixture, view, adds[0], 0).await;
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("conflicts.sqlite");
    std::fs::write(&file, b"not a database, not even close to one").unwrap();
    let index = ConflictIndex::open(&file).unwrap();
    assert_eq!(check(&fixture, &index, view).await.len(), 1);
}

/// Once caught up, a page reads the view's producing application and nothing else
/// from the store, however long the history.
#[tokio::test]
async fn a_page_reads_no_history_once_caught_up() {
    let fixture = Fixture::new().await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let (mut view, adds) = history(&fixture, 60).await;
    for file in [3, 30, 50] {
        view = break_file(&fixture, view, adds[file], file).await;
    }
    let reader = fixture.reader();
    let record = reader.view(view).await.unwrap();
    index.catch_up(&reader, &record).await.unwrap();
    let start = fixture.objects.reads.load(Ordering::Relaxed);
    let page = index
        .conflicts(&reader, &record, None, limit(10))
        .await
        .unwrap();
    let reads = fixture.objects.reads.load(Ordering::Relaxed) - start;
    assert_eq!(page.conflicts.len(), 3);
    // Twice the producing application (catch-up and the read): an application map
    // lookup each, a few nodes deep. Walking would read at least 120 applications.
    assert!(reads <= 12, "a caught-up page read {reads} objects");
}

/// A file one edit writes: its path, the file it expects, and what it writes.
type Written<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);

/// One revision's edits writing each file.
fn exact_all(files: &[Written<'_>]) -> RevisionEdits {
    let mut all = RevisionEdits {
        edits: Default::default(),
        transition: EditTransition::default(),
    };
    for (path, expected, replacement) in files {
        let edits = exact(path, *expected, replacement);
        all.transition.added.extend(edits.transition.added);
        all.edits.extend(edits.edits);
    }
    all
}

fn file(bytes: &[u8]) -> Option<FileValue> {
    Some(FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    })
}

/// A view with two conflicts: change 1 rewrote `p1` and `p2` and added `p3` over
/// what change 0 added, which change 0's revision moved under it; change 3 rewrote
/// `q` over what change 2 added, likewise. Returns the view and the two conflicted
/// changes.
async fn two_conflicts(fixture: &Fixture) -> (ViewId, [ChangeId; 2]) {
    for bytes in ["a1", "a2", "b1", "b2", "c3", "x1", "x2", "qa", "qb", "qx"] {
        fixture.content.put(bytes.as_bytes()).unwrap();
    }
    let (adds, view) = create(
        fixture,
        fixture.base,
        exact_all(&[("p1", None, b"a1"), ("p2", None, b"a2")]),
    )
    .await;
    let (first, view) = create(
        fixture,
        view,
        exact_all(&[
            ("p1", Some(b"a1"), b"b1"),
            ("p2", Some(b"a2"), b"b2"),
            ("p3", None, b"c3"),
        ]),
    )
    .await;
    let (adds_q, view) = create(fixture, view, exact("q", None, b"qa")).await;
    let (second, view) = create(fixture, view, exact("q", Some(b"qa"), b"qb")).await;

    let current = |view, change| async move {
        RevisionMapStore::new(&fixture.objects)
            .get(fixture.reader().view(view).await.unwrap().revisions, change)
            .await
            .unwrap()
            .unwrap()
    };
    let mut view = view;
    for (change, files) in [
        (adds, exact_all(&[("p1", None, b"x1"), ("p2", None, b"x2")])),
        (adds_q, exact("q", None, b"qx")),
    ] {
        let predecessor = current(view, change).await;
        let mut edits = files;
        edits.transition.removed.extend(
            fixture
                .reader()
                .revision(predecessor)
                .await
                .unwrap()
                .edits
                .iter()
                .copied(),
        );
        let (prepared, outcome) = fixture
            .mutator()
            .revise(
                fixture.head(),
                fixture.base,
                view,
                change,
                predecessor,
                RevisionMetadata::default(),
                edits,
            )
            .await
            .unwrap();
        fixture.mutator().publish(prepared).await.unwrap();
        view = outcome.resulting_view;
    }
    (view, [first, second])
}

fn limits(conflicts: usize, edits: usize) -> ConflictPageLimits {
    ConflictPageLimits {
        conflicts: limit(conflicts),
        edits: limit(edits),
    }
}

#[tokio::test]
async fn a_page_of_conflicts_names_each_change_and_what_its_failed_edits_wanted() {
    let fixture = Fixture::new().await;
    let (view, [first, second]) = two_conflicts(&fixture).await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    assert_eq!(browser.conflict_count(&index, view).await.unwrap(), 2);

    let page = browser
        .conflicts(&index, view, None, limits(10, 100))
        .await
        .unwrap();
    assert_eq!(page.next, None);
    let [one, two] = page.items.as_slice() else {
        panic!("two conflicts: {page:?}");
    };
    assert_eq!((one.change, one.position), (first, 1));
    assert_eq!((two.change, two.position), (second, 3));
    assert_eq!(one.title, format!("change {first}"));
    assert_eq!(one.edit_count, 2);
    let mut paths = one
        .edits
        .iter()
        .map(|edit| {
            assert_eq!(edit.reason, EditFailure::ExpectedFileMismatch);
            (edit.path.as_str(), edit.found)
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths, [("p1", file(b"x1")), ("p2", file(b"x2"))]);
    assert_eq!(
        two.edits[0].operation,
        EditOperation::ExactFile {
            expected: file(b"qa"),
            replacement: file(b"qb"),
        }
    );

    // The edit budget: a conflict that doesn't fit is listed with its first edits
    // and ends the page; one that fills it exactly ends it too.
    for (edits, first_page) in [(1, 1), (2, 2)] {
        let page = browser
            .conflicts(&index, view, None, limits(10, edits))
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].edits.len(), first_page);
        assert_eq!(page.items[0].edit_count, 2);
        assert_eq!(page.next, Some(1));
        let rest = browser
            .conflicts(&index, view, page.next, limits(10, edits))
            .await
            .unwrap();
        assert_eq!(rest.items.len(), 1);
        assert_eq!(rest.items[0].change, second);
        assert_eq!(rest.next, None);
    }
}

#[tokio::test]
async fn a_conflict_s_detail_gives_each_path_s_three_files_a_page_at_a_time() {
    let fixture = Fixture::new().await;
    let (view, [first, _]) = two_conflicts(&fixture).await;
    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    assert_eq!(
        browser
            .conflict(&index, view, 0, None, limit(10))
            .await
            .unwrap(),
        None,
        "the change at 0 applied cleanly"
    );

    let detail = browser
        .conflict(&index, view, 1, None, limit(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.change, first);
    assert_eq!(detail.title, format!("change {first}"));
    assert_eq!(detail.path_count, 2, "p3 applied and is not conflicted");
    assert_eq!(detail.next_path.as_deref(), Some("p1"));
    let [p1] = detail.paths.as_slice() else {
        panic!("one path a page: {detail:?}");
    };
    assert_eq!(p1.path, "p1");
    assert_eq!(p1.base, file(b"a1"), "where the change was written");
    assert_eq!(p1.theirs, file(b"b1"), "what it wrote");
    assert_eq!(p1.found, file(b"x1"), "what it met where it landed");
    assert_eq!(
        p1.edits
            .iter()
            .map(|edit| (&edit.operation, &edit.outcome))
            .collect::<Vec<_>>(),
        [(
            &EditOperation::ExactFile {
                expected: file(b"a1"),
                replacement: file(b"b1"),
            },
            &ConflictEditOutcome::Failed(EditFailure::ExpectedFileMismatch)
        )]
    );

    let rest = browser
        .conflict(&index, view, 1, detail.next_path.as_deref(), limit(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rest.paths
            .iter()
            .map(|path| path.path.as_str())
            .collect::<Vec<_>>(),
        ["p2"]
    );
    assert_eq!(rest.next_path, None);
    assert_eq!(rest.paths[0].found, file(b"x2"));
}

/// Twelve numbered lines, with `first` and `last` in place of the first and last.
fn lines(first: &str, last: &str) -> Vec<u8> {
    let middle = [
        "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
    ];
    format!("{first}\n{}\n{last}\n", middle.join("\n")).into_bytes()
}

#[tokio::test]
async fn a_conflicted_path_lists_the_change_s_edits_on_it_that_applied_too() {
    // The change edits the first and the last line; the revised change before it
    // rewrites the last, so the first line's edit applies and the last line's fails.
    let fixture = Fixture::new().await;
    let (original, upstream, after) = (
        lines("one", "twelve"),
        lines("one", "zwölf"),
        lines("ONE", "TWELVE"),
    );
    for bytes in [&original, &upstream, &after] {
        fixture.content.put(bytes).unwrap();
    }
    let (parent, view) = create(&fixture, fixture.base, exact("file", None, &original)).await;
    let snapshot = |bytes| edit_capture::FileSnapshot {
        kind: FileKind::Regular,
        bytes,
    };
    let captured = edit_capture::capture_edit(
        "file",
        Some(snapshot(&original)),
        Some(snapshot(&after)),
        None,
        guarded::Limits::default(),
    )
    .unwrap();
    assert_eq!(captured.edits.len(), 2);
    for value in captured.content.into_values() {
        fixture.content.put(value.bytes()).unwrap();
    }
    let edits = RevisionEdits {
        transition: EditTransition {
            added: captured.edits.keys().copied().collect(),
            ..EditTransition::default()
        },
        edits: captured.edits,
    };
    let (child, view) = create(&fixture, view, edits).await;
    let predecessor = RevisionMapStore::new(&fixture.objects)
        .get(fixture.reader().view(view).await.unwrap().revisions, parent)
        .await
        .unwrap()
        .unwrap();
    let mut revised = exact("file", None, &upstream);
    revised
        .transition
        .removed
        .extend(fixture.reader().revision(predecessor).await.unwrap().edits);
    let (prepared, outcome) = fixture
        .mutator()
        .revise(
            fixture.head(),
            fixture.base,
            view,
            parent,
            predecessor,
            RevisionMetadata::default(),
            revised,
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();

    let directory = tempfile::tempdir().unwrap();
    let index = ConflictIndex::open(&directory.path().join("conflicts.sqlite")).unwrap();
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let detail = browser
        .conflict(&index, outcome.resulting_view, 1, None, limit(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.change, child);
    let [path] = detail.paths.as_slice() else {
        panic!("one conflicted path: {detail:?}");
    };
    assert_eq!(
        (path.base, path.theirs, path.found),
        (file(&original), file(&after), file(&upstream))
    );
    let mut outcomes = path
        .edits
        .iter()
        .map(|edit| matches!(edit.outcome, ConflictEditOutcome::Applied))
        .collect::<Vec<_>>();
    outcomes.sort();
    assert_eq!(outcomes, [false, true], "one failed, one applied");
}
