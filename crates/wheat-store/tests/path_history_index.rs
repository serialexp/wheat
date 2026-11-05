//! The path history index: which changes in a view touch a file or a directory,
//! checked against the changes the test made, and what a page costs.

use crate::support;

use std::{collections::BTreeMap, num::NonZeroUsize, sync::atomic::Ordering};

use support::*;
use wheat_core::*;
use wheat_store::*;

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

/// A change writing `bytes` to `path` on top of `effective`, where the file holds
/// `expected`. Returns the change and the view it makes.
async fn write(
    fixture: &Fixture,
    effective: ViewId,
    path: &str,
    expected: Option<&[u8]>,
    bytes: &[u8],
) -> (ChangeId, ViewId) {
    fixture.content.put(bytes).unwrap();
    let change = fixture.next_change();
    let (prepared, outcome) = fixture
        .mutator()
        .create_on_frontier(
            fixture.head(),
            fixture.base,
            effective,
            ChangeProposal {
                id: change,
                metadata: ChangeMetadata::default(),
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata::default(),
                edits: exact(path, expected, bytes),
            },
        )
        .await
        .unwrap();
    fixture.mutator().publish(prepared).await.unwrap();
    (change, outcome.resulting_view)
}

/// One line of history the test wrote, and what each change did.
#[derive(Clone)]
struct Line {
    tip: ViewId,
    files: BTreeMap<String, Vec<u8>>,
    /// Each change's position, ID, and the file it changed.
    changes: Vec<(u64, ChangeId, String)>,
}

impl Line {
    fn new(fixture: &Fixture) -> Self {
        Self {
            tip: fixture.base,
            files: BTreeMap::new(),
            changes: Vec::new(),
        }
    }

    /// Writes `bytes`, which `path` does not hold, to `path`.
    async fn write(&mut self, fixture: &Fixture, path: &str, bytes: Vec<u8>) {
        let expected = self.files.get(path).cloned();
        let (change, view) = write(fixture, self.tip, path, expected.as_deref(), &bytes).await;
        self.changes
            .push((self.changes.len() as u64, change, path.to_owned()));
        self.files.insert(path.to_owned(), bytes);
        self.tip = view;
    }

    /// The changes that changed `path` or a file below it, newest first.
    fn expected(&self, path: &str) -> Vec<(u64, ChangeId)> {
        let below = format!("{path}/");
        self.changes
            .iter()
            .rev()
            .filter(|(_, _, file)| file == path || file.starts_with(&below))
            .map(|(position, change, _)| (*position, *change))
            .collect()
    }

    /// Every file and directory the line wrote.
    fn paths(&self) -> Vec<String> {
        let mut paths = std::collections::BTreeSet::new();
        for (_, _, file) in &self.changes {
            let mut end = file.len();
            paths.insert(file.clone());
            while let Some(slash) = file[..end].rfind('/') {
                paths.insert(file[..slash].to_owned());
                end = slash;
            }
        }
        paths.into_iter().collect()
    }
}

/// A small deterministic generator, so a failure repeats.
struct Random(u64);

impl Random {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }

    /// A file in a three-level tree, or at the top.
    fn path(&mut self) -> String {
        if self.below(5) == 0 {
            return format!("top{}", self.below(3));
        }
        format!("d{}/s{}/f{}", self.below(3), self.below(3), self.below(4))
    }
}

/// `path`'s whole history in `view`, `size` at a time, as positions and changes.
async fn history(
    browser: &RepositoryBrowser<'_, CountingStore>,
    index: &PathHistoryIndex,
    view: ViewId,
    path: &str,
    size: usize,
) -> Vec<(u64, ChangeId)> {
    let mut items = Vec::new();
    let mut before = None;
    loop {
        let page = browser
            .path_history(index, view, path, before, limit(size))
            .await
            .unwrap();
        assert!(page.items.len() <= size);
        assert!(
            page.next.is_none() || page.items.len() == size,
            "only a full page has a next one"
        );
        items.extend(page.items.iter().map(|item| (item.position, item.change)));
        match page.next {
            Some(next) => before = Some(next),
            None => return items,
        }
    }
}

#[tokio::test]
async fn a_path_s_history_is_the_changes_that_changed_it_or_below_it() {
    let fixture = Fixture::new().await;
    let mut random = Random(7);
    let mut main = Line::new(&fixture);
    let mut fork = None;
    for index in 0..40 {
        if index == 20 {
            // A fork from here: the store holds both lines' rows for the same paths
            // and positions.
            fork = Some(main.clone());
        }
        let path = random.path();
        main.write(&fixture, &path, format!("{path} {index}").into_bytes())
            .await;
    }
    let mut fork = fork.unwrap();
    for index in 0..15 {
        let path = random.path();
        fork.write(
            &fixture,
            &path,
            format!("{path} {}", 100 + index).into_bytes(),
        )
        .await;
    }

    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    for line in [&main, &fork] {
        for path in line.paths() {
            assert_eq!(
                history(&browser, &index, line.tip, &path, 3).await,
                line.expected(&path),
                "{path}"
            );
        }
        assert_eq!(history(&browser, &index, line.tip, "absent", 3).await, []);
    }
}
/// A line of `count` changes, each writing `dir/f{i}`, except every `every`th, which
/// writes `rare`.
async fn sparse_line(fixture: &Fixture, count: usize, every: usize) -> Line {
    let mut line = Line::new(fixture);
    for index in 0..count {
        let path = if index % every == 0 {
            "rare".to_owned()
        } else {
            format!("dir/f{index}")
        };
        line.write(fixture, &path, format!("{index}").into_bytes())
            .await;
    }
    line
}

/// Once caught up, a page reads the objects of its own changes and a membership
/// lookup for each, not the changes between them.
#[tokio::test]
async fn a_page_reads_objects_for_its_changes_not_the_history_between_them() {
    let fixture = Fixture::new().await;
    let line = sparse_line(&fixture, 200, 50).await;
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let view = fixture.reader().view(line.tip).await.unwrap();
    assert_eq!(index.catch_up(&fixture.reader(), &view).await.unwrap(), 200);

    let reads = |path: &'static str, size| {
        let (fixture, browser, index) = (&fixture, &browser, &index);
        async move {
            let start = fixture.objects.reads.load(Ordering::Relaxed);
            let page = browser
                .path_history(index, line.tip, path, None, limit(size))
                .await
                .unwrap();
            (
                page.items.len(),
                fixture.objects.reads.load(Ordering::Relaxed) - start,
            )
        }
    };

    // Each item reads its application, revision and change and a change-map node or
    // two; each candidate a node or two of the view's application map; the view check
    // a few more. Walking the history would read at least 3 × 200 objects.
    let (rare, rare_reads) = reads("rare", 10).await;
    assert_eq!(rare, 4);
    assert!(
        rare_reads <= 4 * 8 + 5,
        "rare's history read {rare_reads} objects"
    );
    let (dir, dir_reads) = reads("dir", 5).await;
    assert_eq!(dir, 5);
    assert!(
        dir_reads <= 6 * 8 + 5,
        "dir's first page read {dir_reads} objects"
    );
}

#[tokio::test]
async fn catching_up_indexes_only_what_the_index_lacks() {
    let fixture = Fixture::new().await;
    let directory = tempfile::tempdir().unwrap();
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let reader = fixture.reader();
    let mut line = sparse_line(&fixture, 10, 3).await;
    let view = reader.view(line.tip).await.unwrap();
    assert_eq!(index.catch_up(&reader, &view).await.unwrap(), 10);
    assert_eq!(index.catch_up(&reader, &view).await.unwrap(), 0);
    for index in 10..15 {
        line.write(&fixture, "rare", format!("{index}").into_bytes())
            .await;
    }
    let view = reader.view(line.tip).await.unwrap();
    assert_eq!(index.catch_up(&reader, &view).await.unwrap(), 5);
    // An empty view has nothing to index.
    let base = reader.view(fixture.base).await.unwrap();
    assert_eq!(index.catch_up(&reader, &base).await.unwrap(), 0);
}

#[tokio::test]
async fn an_unreadable_index_file_is_emptied_and_built_again() {
    let fixture = Fixture::new().await;
    let line = sparse_line(&fixture, 12, 4).await;
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("paths.sqlite");
    std::fs::write(&file, b"not a database, not even close to one").unwrap();
    let index = PathHistoryIndex::open(&file).unwrap();
    assert_eq!(
        history(&browser, &index, line.tip, "rare", 2).await,
        line.expected("rare")
    );
}

#[tokio::test]
async fn the_root_s_history_is_the_view_s_and_bad_arguments_are_refused() {
    let fixture = Fixture::new().await;
    let line = sparse_line(&fixture, 6, 2).await;
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();

    assert_eq!(
        browser
            .path_history(&index, line.tip, "", None, limit(10))
            .await
            .unwrap(),
        browser.history(line.tip, None, limit(10)).await.unwrap()
    );
    assert!(matches!(
        browser
            .path_history(&index, line.tip, "dir//f1", None, limit(10))
            .await,
        Err(BrowseError::InvalidPath(_))
    ));
    assert!(matches!(
        browser
            .path_history(&index, line.tip, "rare", Some(7), limit(10))
            .await,
        Err(BrowseError::PositionOutOfRange {
            position: 7,
            size: 6
        })
    ));
    // A page from a position keeps to the changes before it.
    let page = browser
        .path_history(&index, line.tip, "rare", Some(4), limit(10))
        .await
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>(),
        [2, 0]
    );
    assert_eq!(page.next, None);
}
