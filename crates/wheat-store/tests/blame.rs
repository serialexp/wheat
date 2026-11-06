//! Blame: which change last wrote each line of a file, checked against the change
//! the test wrote each line in, its bound, and what it reads.

use crate::support;

use std::{collections::BTreeMap, num::NonZeroUsize, sync::atomic::Ordering};

use support::*;
use wheat_core::*;
use wheat_store::*;

fn limits(max_changes: usize) -> BlameLimits {
    BlameLimits {
        max_bytes: 1 << 20,
        max_changes: NonZeroUsize::new(max_changes).unwrap(),
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
}

/// One line of history: its tip, the files it holds, and the change that wrote each
/// line of each text file.
#[derive(Clone)]
struct Line {
    tip: ViewId,
    files: BTreeMap<String, Vec<u8>>,
    /// Each text file's lines and, for each, the change that wrote it.
    lines: BTreeMap<String, Vec<(String, ChangeId)>>,
}

impl Line {
    fn new(fixture: &Fixture) -> Self {
        Self {
            tip: fixture.base,
            files: BTreeMap::new(),
            lines: BTreeMap::new(),
        }
    }

    /// Writes `bytes` to `path` and returns the change.
    async fn write(&mut self, fixture: &Fixture, path: &str, bytes: Vec<u8>) -> ChangeId {
        fixture.content.put(&bytes).unwrap();
        let change = fixture.next_change();
        let expected = self.files.get(path).cloned();
        let (prepared, outcome) = fixture
            .mutator()
            .create_on_frontier(
                fixture.head(),
                fixture.base,
                self.tip,
                ChangeProposal {
                    id: change,
                    metadata: ChangeMetadata::default(),
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata::default(),
                    edits: exact(path, expected.as_deref(), &bytes),
                },
            )
            .await
            .unwrap();
        fixture.mutator().publish(prepared).await.unwrap();
        self.files.insert(path.to_owned(), bytes);
        self.tip = outcome.resulting_view;
        change
    }

    /// Writes `lines` as `path`'s text, where the ones `new` marks are this change's.
    async fn write_lines(&mut self, fixture: &Fixture, path: &str, lines: Vec<(String, bool)>) {
        let text: String = lines.iter().map(|(line, _)| line.as_str()).collect();
        let change = self.write(fixture, path, text.into_bytes()).await;
        let old = self.lines.remove(path).unwrap_or_default();
        let mut old = old.into_iter();
        let mut written = Vec::with_capacity(lines.len());
        for (line, new) in lines {
            let origin = if new {
                change
            } else {
                // Kept lines keep their order, so the next kept one is next in `old`.
                loop {
                    let (text, origin) = old.next().expect("a kept line was in the file");
                    if text == line {
                        break origin;
                    }
                }
            };
            written.push((line, origin));
        }
        self.lines.insert(path.to_owned(), written);
    }

    /// A random edit of `path`: lines removed, replaced and inserted, each new line
    /// unique, so the change that wrote each line is known.
    async fn edit(&mut self, fixture: &Fixture, random: &mut Random, path: &str, serial: &mut u64) {
        let old = self.lines.get(path).cloned().unwrap_or_default();
        let mut lines = Vec::new();
        for (text, _) in old {
            match random.below(10) {
                0 => {}
                1 => lines.push(fresh(serial)),
                2 => {
                    lines.push(fresh(serial));
                    lines.push((text, false));
                }
                _ => lines.push((text, false)),
            }
        }
        for _ in 0..random.below(4) {
            let at = random.below(lines.len() as u64 + 1) as usize;
            lines.insert(at, fresh(serial));
        }
        if lines.is_empty() {
            lines.push(fresh(serial));
        }
        self.write_lines(fixture, path, lines).await;
    }
}

/// A new line, unlike every other.
fn fresh(serial: &mut u64) -> (String, bool) {
    *serial += 1;
    (format!("line {serial}\n"), true)
}

/// Each line's origin change, as `blame` gives it.
fn origins(blame: &Blame<HistoryItem>) -> Vec<Option<ChangeId>> {
    let mut lines = Vec::new();
    let mut next = 0;
    for range in &blame.ranges {
        assert_eq!(
            range.start, next,
            "ranges are in order and cover every line"
        );
        assert!(range.len > 0);
        next += range.len;
        let origin = match range.origin {
            BlameOrigin::Change { index } => Some(blame.origins[index].change),
            BlameOrigin::Unresolved { .. } => None,
        };
        lines.extend(std::iter::repeat_n(origin, range.len as usize));
    }
    assert_eq!(next, blame.lines);
    for pair in blame.ranges.windows(2) {
        assert_ne!(pair[0].origin, pair[1].origin, "neighbours are merged");
    }
    let mut positions: Vec<_> = blame.origins.iter().map(|item| item.position).collect();
    positions.dedup();
    assert!(
        positions.windows(2).all(|pair| pair[0] > pair[1]),
        "newest first"
    );
    lines
}

async fn open(fixture: &Fixture) -> (RepositoryBrowser<'_, CountingStore>, tempfile::TempDir) {
    let browser = RepositoryBrowser::open(&fixture.objects, &fixture.content, fixture.head())
        .await
        .unwrap();
    (browser, tempfile::tempdir().unwrap())
}

#[tokio::test]
async fn every_line_is_the_change_that_wrote_it() {
    let fixture = Fixture::new().await;
    let mut random = Random(11);
    let mut serial = 0;
    let mut main = Line::new(&fixture);
    let mut fork = None;
    let paths = ["a.txt", "d/b.txt", "d/e/c.txt"];
    for index in 0..60 {
        if index == 30 {
            fork = Some(main.clone());
        }
        let path = paths[random.below(3) as usize];
        main.edit(&fixture, &mut random, path, &mut serial).await;
    }
    let mut fork = fork.unwrap();
    for _ in 0..20 {
        let path = paths[random.below(3) as usize];
        fork.edit(&fixture, &mut random, path, &mut serial).await;
    }

    let (browser, directory) = open(&fixture).await;
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    for line in [&main, &fork] {
        for (path, expected) in &line.lines {
            let blame = browser
                .blame(&index, line.tip, path, limits(1000))
                .await
                .unwrap()
                .unwrap();
            let expected: Vec<_> = expected.iter().map(|(_, change)| Some(*change)).collect();
            assert_eq!(origins(&blame), expected, "{path}");
        }
    }
}

#[tokio::test]
async fn at_its_bound_a_blame_says_where_the_rest_carries_on() {
    let fixture = Fixture::new().await;
    let mut random = Random(5);
    let mut serial = 0;
    let mut line = Line::new(&fixture);
    for _ in 0..12 {
        line.edit(&fixture, &mut random, "f", &mut serial).await;
    }
    let (browser, directory) = open(&fixture).await;
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let full = browser
        .blame(&index, line.tip, "f", limits(100))
        .await
        .unwrap()
        .unwrap();
    let full_origins = origins(&full);
    assert!(full_origins.iter().all(Option::is_some));

    let bounded = browser
        .blame(&index, line.tip, "f", limits(3))
        .await
        .unwrap()
        .unwrap();
    assert!(bounded.origins.len() <= 3);
    let bounded_origins = origins(&bounded);
    let mut unresolved = 0;
    for range in &bounded.ranges {
        let BlameOrigin::Unresolved { before, line: at } = range.origin else {
            continue;
        };
        unresolved += range.len;
        // The rest of the walk, from where this one stopped, finds what the full
        // walk did for these lines.
        let rest = browser
            .blame(&index, before, "f", limits(100))
            .await
            .unwrap()
            .unwrap();
        let rest_origins = origins(&rest);
        for offset in 0..range.len {
            assert_eq!(
                rest_origins[(at + offset) as usize],
                full_origins[(range.start + offset) as usize]
            );
        }
    }
    assert!(unresolved > 0, "three changes do not resolve this file");
    for (bounded, full) in bounded_origins.iter().zip(&full_origins) {
        if bounded.is_some() {
            assert_eq!(bounded, full);
        }
    }
}

#[tokio::test]
async fn a_version_that_was_not_text_ends_the_walk() {
    let fixture = Fixture::new().await;
    let mut line = Line::new(&fixture);
    line.write(&fixture, "f", vec![0, 1, 2, b'\n']).await;
    line.write_lines(
        &fixture,
        "f",
        vec![("one\n".into(), true), ("two".into(), true)],
    )
    .await;
    line.write_lines(
        &fixture,
        "f",
        vec![
            ("one\n".into(), false),
            ("three\n".into(), true),
            ("two".into(), false),
        ],
    )
    .await;
    let (browser, directory) = open(&fixture).await;
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let blame = browser
        .blame(&index, line.tip, "f", limits(100))
        .await
        .unwrap()
        .unwrap();
    let expected: Vec<_> = line.lines["f"].iter().map(|(_, c)| Some(*c)).collect();
    assert_eq!(origins(&blame), expected);
    assert_eq!(blame.lines, 3);
    assert_eq!(blame.origins.len(), 2);
}

#[tokio::test]
async fn files_a_blame_cannot_read_are_refused() {
    let fixture = Fixture::new().await;
    let mut line = Line::new(&fixture);
    line.write(&fixture, "bin", vec![0, 159, 146, 150]).await;
    line.write(&fixture, "d/long", vec![b'x'; 2000]).await;
    line.write(&fixture, "empty", Vec::new()).await;
    let (browser, directory) = open(&fixture).await;
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let small = BlameLimits {
        max_bytes: 1000,
        ..limits(10)
    };
    let blame = |path: &'static str| {
        let (browser, index, tip) = (&browser, &index, line.tip);
        async move { browser.blame(index, tip, path, small).await }
    };
    assert!(matches!(
        blame("bin").await,
        Err(BrowseError::Blame(BlameError::NotText))
    ));
    assert!(matches!(
        blame("d/long").await,
        Err(BrowseError::Blame(BlameError::TooLong {
            size: 2000,
            max: 1000
        }))
    ));
    assert!(matches!(
        blame("a//b").await,
        Err(BrowseError::Blame(BlameError::InvalidPath(_)))
    ));
    assert_eq!(blame("absent").await.unwrap(), None);
    assert_eq!(blame("d").await.unwrap(), None);
    assert_eq!(blame("").await.unwrap(), None);
    let empty = blame("empty").await.unwrap().unwrap();
    assert_eq!(
        (empty.lines, empty.ranges.len(), empty.origins.len()),
        (0, 0, 0)
    );
}

/// A blame reads objects for the file's own changes, not the history between them.
#[tokio::test]
async fn a_blame_reads_the_file_s_changes_not_the_history_between_them() {
    let fixture = Fixture::new().await;
    let mut random = Random(3);
    let mut serial = 0;
    let mut line = Line::new(&fixture);
    for index in 0..200 {
        if index % 50 == 0 {
            line.edit(&fixture, &mut random, "d/rare", &mut serial)
                .await;
        } else {
            line.write(
                &fixture,
                &format!("d/f{index}"),
                format!("{index}").into_bytes(),
            )
            .await;
        }
    }
    let (browser, directory) = open(&fixture).await;
    let index = PathHistoryIndex::open(&directory.path().join("paths.sqlite")).unwrap();
    let view = fixture.reader().view(line.tip).await.unwrap();
    assert_eq!(index.catch_up(&fixture.reader(), &view).await.unwrap(), 200);

    let start = fixture.objects.reads.load(Ordering::Relaxed);
    let blame = browser
        .blame(&index, line.tip, "d/rare", limits(100))
        .await
        .unwrap()
        .unwrap();
    let reads = fixture.objects.reads.load(Ordering::Relaxed) - start;
    let expected: Vec<_> = line.lines["d/rare"].iter().map(|(_, c)| Some(*c)).collect();
    assert_eq!(origins(&blame), expected);
    // Each of the 4 changes walked reads its application, its parent view, the path's
    // nodes in that view's tree and a node or two of the view's application map; each
    // origin a history item's revision, change and change-map nodes; the view and its
    // lookup a few more. Walking the history would read at least 3 × 200 objects.
    assert!(
        reads <= 4 * 10 + 4 * 6 + 8,
        "the blame read {reads} objects"
    );
}
