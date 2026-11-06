//! A state's tree roots and edits, a page at a time, as Gothab's secret sweep reads
//! them: complete (together they name every content the state requires), in the same
//! order on every walk, and at a bounded number of reads a page.

use crate::support;

use std::{collections::BTreeSet, num::NonZeroUsize, sync::atomic::Ordering};

use support::*;
use wheat_core::*;
use wheat_store::*;

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

async fn browser(fixture: &Fixture) -> RepositoryBrowser<'_, CountingStore> {
    RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap()
}

async fn all_trees(browser: &RepositoryBrowser<'_, CountingStore>, size: usize) -> Vec<StateTree> {
    let mut trees = Vec::new();
    let mut from = None;
    loop {
        let page = browser.state_trees(from, limit(size)).await.unwrap();
        assert!(page.trees.len() <= size);
        trees.extend(page.trees);
        match page.next {
            Some(next) => from = Some(next),
            None => return trees,
        }
    }
}

async fn all_edits(browser: &RepositoryBrowser<'_, CountingStore>, size: usize) -> Vec<StateEdit> {
    let mut edits = Vec::new();
    let mut from = None;
    loop {
        let page = browser.state_edits(from, limit(size)).await.unwrap();
        assert!(page.edits.len() <= size);
        edits.extend(page.edits);
        match page.next {
            Some(next) => from = Some(next),
            None => return edits,
        }
    }
}

/// A change `a` writing `x`, a change `c` on it writing `a`, then `a` revised to
/// write `a` itself, so `c` conflicts when it is applied again. The first revision
/// of `a` is superseded but still retained.
async fn conflicted(fixture: &Fixture) -> (ChangeId, ChangeId) {
    let (a, after_a) = fixture
        .create(fixture.base, fixture.base, "x", b"one")
        .await;
    let (c, after_c) = fixture.create(fixture.base, after_a, "a", b"two").await;
    revise(
        fixture,
        fixture.base,
        after_c,
        a,
        Revision::Replace(b"other"),
        1,
    )
    .await;
    (a, c)
}

#[tokio::test]
async fn the_trees_and_edits_together_name_every_content_the_state_requires() {
    let fixture = Fixture::new().await;
    let (a, c) = conflicted(&fixture).await;
    let browser = browser(&fixture).await;

    let trees = all_trees(&browser, 2).await;
    let edits = all_edits(&browser, 1).await;
    // Paging changes nothing, and a second walk is the same walk.
    assert_eq!(trees, all_trees(&browser, 1000).await);
    assert_eq!(edits, all_edits(&browser, 1000).await);
    assert_eq!(edits, all_edits(&browser, 2).await);

    let revisions_of = |change| {
        trees
            .iter()
            .filter(|tree| matches!(tree.origin, TreeOrigin::Revision { change: of, .. } if of == change))
            .count()
    };
    assert_eq!(revisions_of(a), 2, "a's superseded revision is listed too");
    assert_eq!(revisions_of(c), 1);
    assert!(trees.iter().any(|tree| matches!(
        tree.origin,
        TreeOrigin::Application {
            conflicted: true,
            ..
        }
    )));
    let conflict = edits
        .iter()
        .find(|edit| matches!(edit.origin, EditOrigin::Conflict { .. }))
        .expect("c's failed edit is listed");
    assert_eq!(conflict.path, "a");
    assert_eq!(conflict.contents, [ContentId::of(b"other")]);
    let EditOrigin::Conflict { revision, .. } = conflict.origin else {
        unreachable!()
    };
    assert_eq!(
        fixture.reader().revision(revision).await.unwrap().change_id,
        c
    );
    assert!(edits.iter().any(|edit| edit.path == "x"
        && edit.contents == [ContentId::of(b"one")]
        && matches!(edit.origin, EditOrigin::Revision(_))));

    // Everything the trees and edits name is what the state requires, no more.
    let trees_store = MerkleTreeStore::new(&fixture.objects);
    let mut named = BTreeSet::new();
    for tree in &trees {
        let mut cursor = trees_store
            .content_cursor(tree.tree, TreeContentWalkLimits::default())
            .await
            .unwrap();
        while let Some(value) = cursor.next().await.unwrap() {
            named.insert(value.content);
        }
    }
    named.extend(edits.iter().flat_map(|edit| edit.contents.iter().copied()));
    let reader = fixture.reader();
    let state = reader
        .state(
            reader
                .envelope(fixture.head().envelope)
                .await
                .unwrap()
                .state,
        )
        .await
        .unwrap();
    let mut required = BTreeSet::new();
    let mut cursor = ContentSetStore::new(&fixture.objects)
        .cursor(state.required_content)
        .await
        .unwrap();
    while let Some(id) = cursor.next().await.unwrap() {
        required.insert(id);
    }
    assert_eq!(named, required);
}

#[tokio::test]
async fn a_new_repository_has_its_base_tree_and_no_edits() {
    let fixture = Fixture::new().await;
    let browser = browser(&fixture).await;
    let page = browser.state_trees(None, limit(1)).await.unwrap();
    assert_eq!(page.next, None, "a full last page ends the walk");
    assert_eq!(
        page.trees
            .iter()
            .map(|tree| tree.origin)
            .collect::<Vec<_>>(),
        [TreeOrigin::View(fixture.base)]
    );
    assert_eq!(
        browser.state_edits(None, limit(1)).await.unwrap(),
        StateEditPage::default()
    );
}

/// A history of `count` changes on one line, each writing `dir/f{i}`.
async fn line(fixture: &Fixture, count: usize) {
    let mut tip = fixture.base;
    for index in 0..count {
        tip = fixture
            .create(
                fixture.base,
                tip,
                &format!("dir/f{index}"),
                format!("v{index}").as_bytes(),
            )
            .await
            .1;
    }
}

/// A page deep in the walk reads about what the first page does: its own records
/// plus the set nodes on the way to its start, not every record before it.
#[tokio::test]
async fn a_page_reads_a_bounded_number_of_objects() {
    const CHANGES: usize = 96;
    const PAGE: usize = 4;
    let fixture = Fixture::new().await;
    line(&fixture, CHANGES).await;
    let browser = browser(&fixture).await;
    let reads = || fixture.objects.reads.load(Ordering::Relaxed);

    // Views, revisions and applications: three sets of about 96 IDs each.
    let trees = all_trees(&browser, PAGE).await;
    assert_eq!(trees.len(), 1 + 3 * CHANGES);
    let edits = all_edits(&browser, PAGE).await;
    assert_eq!(edits.len(), CHANGES);

    // A record per item, and per set the nodes on the path to the start (a radix
    // set of 96 IDs is two or three levels) plus those the page walks through.
    let bound = PAGE + 4 * (CHANGES.ilog2() as usize);
    let mut from = None;
    let mut pages = 0;
    loop {
        let start = reads();
        let page = browser.state_trees(from, limit(PAGE)).await.unwrap();
        let read = reads() - start;
        assert!(read <= bound, "tree page {pages} read {read} objects");
        pages += 1;
        match page.next {
            Some(next) => from = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, trees.len().div_ceil(PAGE));

    // An edit page reads each revision or application it visits, and each edit;
    // the clean applications after the revisions cost at most a page's worth each.
    let mut from = None;
    let mut pages = 0;
    loop {
        let start = reads();
        let page = browser.state_edits(from, limit(PAGE)).await.unwrap();
        let read = reads() - start;
        assert!(
            read <= PAGE + bound,
            "edit page {pages} read {read} objects"
        );
        pages += 1;
        match page.next {
            Some(next) => from = Some(next),
            None => break,
        }
    }
    // The revisions, then the applications PAGE at a time, then one empty page.
    assert!(
        pages <= 2 * CHANGES.div_ceil(PAGE) + 1,
        "{pages} edit pages"
    );
}
