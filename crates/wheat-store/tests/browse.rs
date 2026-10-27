//! Browsing a repository without a workspace: directories, files and history pages.

mod support;

use std::{num::NonZeroUsize, sync::atomic::Ordering};

use support::*;
use wheat_core::*;
use wheat_store::*;

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn file(bytes: &[u8]) -> DirectoryEntry {
    DirectoryEntry::File(FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    })
}

/// A root holding `wide/` with `count` files and `top`, a file.
async fn wide_tree(objects: &impl ImmutableObjectStore, count: usize) -> TreeId {
    let trees = MerkleTreeStore::new(objects);
    let wide = DirectoryNode {
        entries: (0..count)
            .map(|index| (format!("f{index:04}"), file(index.to_string().as_bytes())))
            .collect(),
    };
    let wide = trees.write_node(&wide).await.unwrap();
    trees
        .write_node(&DirectoryNode {
            entries: [
                ("top".to_owned(), file(b"top")),
                ("wide".to_owned(), DirectoryEntry::Directory(wide)),
            ]
            .into(),
        })
        .await
        .unwrap()
}

fn path(components: &[&str]) -> Vec<String> {
    components.iter().map(|&part| part.to_owned()).collect()
}

#[tokio::test]
async fn a_wide_directory_lists_in_name_order_a_page_at_a_time() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let root = wide_tree(&objects, 300).await;
    let trees = MerkleTreeStore::new(&objects);

    let mut names = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let TreeListing::Directory(page) = trees
            .list_directory(root, &path(&["wide"]), after.as_deref(), limit(128))
            .await
            .unwrap()
        else {
            panic!("wide is a directory");
        };
        pages += 1;
        assert!(page.entries.len() <= 128);
        names.extend(page.entries.into_iter().map(|(name, _)| name));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }

    assert_eq!(pages, 3);
    assert_eq!(
        names,
        (0..300)
            .map(|index| format!("f{index:04}"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_listing_tells_files_and_absent_paths_apart() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let root = wide_tree(&objects, 3).await;
    let trees = MerkleTreeStore::new(&objects);
    let list = |components: &'static [&'static str]| {
        let trees = &trees;
        async move {
            trees
                .list_directory(root, &path(components), None, limit(10))
                .await
                .unwrap()
        }
    };

    let TreeListing::Directory(top) = list(&[]).await else {
        panic!("the root is a directory");
    };
    assert_eq!(
        top.entries
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["top", "wide"]
    );
    assert_eq!(top.next, None);
    assert!(
        matches!(list(&["top"]).await, TreeListing::File(value) if value.content == ContentId::of(b"top"))
    );
    assert_eq!(list(&["missing"]).await, TreeListing::Absent);
    assert_eq!(list(&["top", "below"]).await, TreeListing::Absent);
    // A page that ends exactly at the last entry has no next page.
    let TreeListing::Directory(exact) = trees
        .list_directory(root, &path(&["wide"]), Some("f0000"), limit(2))
        .await
        .unwrap()
    else {
        panic!("wide is a directory");
    };
    assert_eq!(exact.entries.len(), 2);
    assert_eq!(exact.next, None);
}

/// A history of `count` changes on one line, each writing `dir/f{i}`. Returns each
/// change's view, oldest first.
async fn line(fixture: &Fixture, count: usize) -> Vec<(ChangeId, ViewId)> {
    let mut views = Vec::with_capacity(count);
    let mut tip = fixture.base;
    for index in 0..count {
        let (change, view) = fixture
            .create(
                fixture.base,
                tip,
                &format!("dir/f{index}"),
                format!("v{index}").as_bytes(),
            )
            .await;
        views.push((change, view));
        tip = view;
    }
    views
}

async fn browser(fixture: &Fixture) -> RepositoryBrowser<'_, CountingStore> {
    RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap()
}

#[tokio::test]
async fn history_pages_walk_a_view_newest_first() {
    let fixture = Fixture::new().await;
    let views = line(&fixture, 12).await;
    let tip = views.last().unwrap().1;
    let browser = browser(&fixture).await;

    let mut items = Vec::new();
    let mut before = None;
    loop {
        let page = browser.history(tip, before, limit(5)).await.unwrap();
        assert!(page.items.len() <= 5);
        items.extend(page.items);
        match page.next {
            Some(next) => before = Some(next),
            None => break,
        }
    }

    assert_eq!(
        items.iter().map(|item| item.position).collect::<Vec<_>>(),
        (0..12).rev().collect::<Vec<u64>>()
    );
    for (item, index) in items.iter().zip((0..12).rev()) {
        assert_eq!(item.change, views[index].0);
        let parent = index.checked_sub(1).map_or(fixture.base, |at| views[at].1);
        assert_eq!(item.parent, parent);
        assert_eq!(
            item.tree,
            fixture.reader().view(views[index].1).await.unwrap().tree
        );
        assert!(!item.conflicted);
    }
    assert_eq!(
        browser.history(tip, Some(0), limit(5)).await.unwrap(),
        HistoryPage::default()
    );
    assert!(matches!(
        browser.history(tip, Some(13), limit(5)).await,
        Err(BrowseError::PositionOutOfRange {
            position: 13,
            size: 12
        })
    ));
}

/// A page reads the objects of its own changes plus O(log n) to find the first, so a
/// page deep in a long history costs about what the newest page does.
#[tokio::test]
async fn a_history_page_reads_a_bounded_number_of_objects() {
    let fixture = Fixture::new().await;
    let views = line(&fixture, 96).await;
    let tip = views.last().unwrap().1;
    let browser = browser(&fixture).await;
    let reads = |before| {
        let fixture = &fixture;
        let browser = &browser;
        async move {
            let start = fixture.objects.reads.load(Ordering::Relaxed);
            let page = browser.history(tip, before, limit(4)).await.unwrap();
            assert_eq!(page.items.len(), 4);
            fixture.objects.reads.load(Ordering::Relaxed) - start
        }
    };

    let newest = reads(None).await;
    let deep = reads(Some(40)).await;

    // Each item reads its application, revision and change, plus a change-map lookup of
    // a node or two; the view check and the skip chain to the first item add O(log n).
    // Reading every change would be at least 3 × 96 objects.
    let bound = 4 * 5 + 96_u64.ilog2() as usize + 3;
    assert!(newest <= bound, "newest page read {newest} objects");
    assert!(deep <= bound, "a page at position 40 read {deep} objects");
}

/// A history of `times.len()` changes on one line, the `i`th committed at `times[i]`.
/// Returns each change, oldest first in canonical order, and the tip.
async fn timed_line(fixture: &Fixture, times: &[i64]) -> (Vec<ChangeId>, ViewId) {
    let mut changes = Vec::with_capacity(times.len());
    let mut tip = fixture.base;
    for (index, time) in times.iter().enumerate() {
        let (change, view) = fixture
            .create_committed_at(
                fixture.base,
                tip,
                &format!("dir/f{index}"),
                format!("v{index}").as_bytes(),
                *time,
            )
            .await;
        changes.push(change);
        tip = view;
    }
    (changes, tip)
}

/// Every change of `tip`, a page of `page` at a time, by commit time.
async fn pages_by_time(
    browser: &RepositoryBrowser<'_, CountingStore>,
    tip: ViewId,
    page: usize,
) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    let mut from = None;
    loop {
        let page = browser
            .history_by_time(tip, from, limit(page))
            .await
            .unwrap();
        assert!(!page.items.is_empty());
        items.extend(page.items);
        match page.next {
            Some(next) => from = Some(next),
            None => return items,
        }
    }
}

/// Paging by time follows commit times, not the order changes were made in: newest
/// first, and within one second by highest change ID first.
#[tokio::test]
async fn history_pages_by_time_walk_a_view_newest_committed_first() {
    let fixture = Fixture::new().await;
    let times = [
        50,
        -20,
        900,
        50,
        7,
        900,
        0,
        1_700_000_000,
        50,
        -3_000_000_000,
        7,
    ];
    let (changes, tip) = timed_line(&fixture, &times).await;
    let browser = browser(&fixture).await;

    let mut expected: Vec<(i64, ChangeId)> = times.iter().copied().zip(changes).collect();
    expected.sort_by(|left, right| right.cmp(left));
    for page in [1, 3, 4, times.len(), times.len() + 1] {
        let items = pages_by_time(&browser, tip, page).await;
        assert_eq!(
            items
                .iter()
                .map(|item| (item.committed_at.seconds, item.change))
                .collect::<Vec<_>>(),
            expected,
            "pages of {page}"
        );
    }
    // Each item is the change as the view holds it, at its place in canonical order.
    let canonical = browser
        .history(tip, None, limit(times.len()))
        .await
        .unwrap();
    for item in pages_by_time(&browser, tip, 5).await {
        assert!(canonical.items.contains(&item), "{item:?}");
    }
    // The base view has no history.
    let base = browser
        .history_by_time(fixture.base, None, limit(3))
        .await
        .unwrap();
    assert_eq!(base, TimeHistoryPage::default());
}

/// A page by time reads O(log n) history-map nodes to find its first change and a few
/// objects per change, so a page deep in a long history costs about what the newest
/// page does.
#[tokio::test]
async fn a_history_page_by_time_reads_a_bounded_number_of_objects() {
    let fixture = Fixture::new().await;
    // Commit times out of canonical order, so the map is not filled in key order.
    let times: Vec<i64> = (0..96).map(|index| (index * 37) % 96).collect();
    let (_, tip) = timed_line(&fixture, &times).await;
    let browser = browser(&fixture).await;
    let middle = pages_by_time(&browser, tip, 96).await[40].time_key();
    let reads = |from| {
        let fixture = &fixture;
        let browser = &browser;
        async move {
            let start = fixture.objects.reads.load(Ordering::Relaxed);
            let page = browser.history_by_time(tip, from, limit(4)).await.unwrap();
            assert_eq!(page.items.len(), 4);
            fixture.objects.reads.load(Ordering::Relaxed) - start
        }
    };

    let newest = reads(None).await;
    let deep = reads(Some(middle)).await;

    // Each item reads its application, revision and change, plus a change-map lookup
    // of a node or two; the view check and the history-map path add O(log n). Reading
    // every change would be at least 3 × 96 objects.
    let bound = 4 * 5 + 96_u64.ilog2() as usize + 3;
    assert!(newest <= bound, "the newest page read {newest} objects");
    assert!(deep <= bound, "a page from the middle read {deep} objects");
}

#[tokio::test]
async fn files_are_read_at_a_path_within_a_bound() {
    let fixture = Fixture::new().await;
    let views = line(&fixture, 2).await;
    let tip = views[1].1;
    let browser = browser(&fixture).await;

    let read = browser
        .read_file(tip, "dir/f1", 1024)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.bytes.as_deref(), Some(&b"v1"[..]));
    assert_eq!(read.size, 2);
    assert_eq!(read.value.content, ContentId::of(b"v1"));

    let large = browser.read_file(tip, "dir/f1", 1).await.unwrap().unwrap();
    assert_eq!((large.size, large.bytes), (2, None));

    for absent in ["", "dir", "dir/missing", "dir/f1/below"] {
        assert_eq!(browser.read_file(tip, absent, 1024).await.unwrap(), None);
    }
    assert!(matches!(
        browser.read_file(tip, "dir//f1", 1024).await,
        Err(BrowseError::InvalidPath(_))
    ));
    // The older view does not have the newer file.
    assert_eq!(
        browser.read_file(views[0].1, "dir/f1", 1024).await.unwrap(),
        None
    );

    let TreeListing::Directory(listing) = browser.list(tip, "dir", None, limit(10)).await.unwrap()
    else {
        panic!("dir is a directory");
    };
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["f0", "f1"]
    );
}

#[tokio::test]
async fn views_the_head_does_not_retain_are_refused() {
    let fixture = Fixture::new().await;
    line(&fixture, 1).await;
    let browser = browser(&fixture).await;
    let stranger = Repository::new(Tree::from([(
        "x".to_owned(),
        FileValue {
            content: ContentId::of(b"x"),
            kind: FileKind::Regular,
        },
    )]))
    .base_view()
    .id;

    assert!(matches!(
        browser.history(stranger, None, limit(1)).await,
        Err(BrowseError::ViewNotRetained(view)) if view == stranger
    ));
    assert!(matches!(
        browser.list(stranger, "", None, limit(1)).await,
        Err(BrowseError::ViewNotRetained(_))
    ));
}

#[tokio::test]
async fn a_new_repository_browses_as_an_empty_labelled_view() {
    let parent = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(parent.path().join("repo"));
    let head = directory.create_empty("main").await.unwrap();
    let objects = directory.open_store().unwrap();
    let content = objects.content_store();
    let browser = RepositoryBrowser::open(&objects, &content, head)
        .await
        .unwrap();

    let main = browser.label("main").unwrap();
    assert_eq!(browser.label("other"), None);
    assert_eq!(
        browser.list(main, "", None, limit(10)).await.unwrap(),
        TreeListing::Directory(DirectoryPage::default())
    );
    assert_eq!(
        browser.history(main, None, limit(10)).await.unwrap(),
        HistoryPage::default()
    );
}
