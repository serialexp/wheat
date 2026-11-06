//! A file's blame in a long linear history, as a host's blame page reads it.
//!
//! The history has Gothab's shape (see `path_history`): change `i` rewrites file
//! `i mod FILES` of [`FILES`], except every [`HOT_EVERY`]th, which edits the hot
//! file: [`HOT_LINES`] lines, of which each edit replaces [`EDITED_LINES`] at
//! pseudo-random places. Change 0 creates it. At each checkpoint a
//! `wheat_store::PathHistoryIndex` catches up to the tip, then the hot file and a
//! cold one (changed every [`FILES`] changes) are blamed [`SAMPLES`] times each,
//! recording the objects the first blame reads and the median wall time; each blame
//! is checked line by line against the change that wrote the line. At [`LARGEST`]
//! changes the hot file's blame walks its 1,000 changes, and fits Gothab's page
//! budget.

use std::{fs, num::NonZeroUsize, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_store::{
    BlameLimits, BlameOrigin, LocalPublicationStore, PathHistoryIndex, PersistedRepositoryMutator,
    RepositoryBrowser, RepositoryReader,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch, change_id, write_at},
};
use crate::measure;

pub(crate) const NAME: &str = "blame";
const SAMPLES: usize = 5;
const FILES: usize = 1_000;
const HOT_EVERY: usize = 20;
const HOT_LINES: usize = 2_000;
const EDITED_LINES: usize = 5;
const HOT: &str = "src/hot.rs";
/// The cold file blamed: the one change 1 writes.
const COLD: usize = 1;
/// The history size the budgets are checked at.
const LARGEST: usize = 20_000;
/// What a blame may walk and read, as a host would ask.
const MAX_CHANGES: usize = 2_000;
const MAX_BYTES: usize = 1 << 20;
/// Per change walked: its application, its parent view, the path's two nodes in that
/// view's tree, and the membership lookup in the view's application map.
const READS_PER_CHANGE: f64 = 10.0;
/// Per change a range names: its history item's revision, change and change-map nodes.
const READS_PER_ORIGIN: f64 = 6.0;
/// Gothab fails a page that takes longer.
const PAGE_BUDGET_SECONDS: f64 = 1.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context.scale.pick(vec![2_000, LARGEST], vec![200, 400]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("files", FILES);
    recorder.parameter("hot_every", HOT_EVERY);
    recorder.parameter("hot_lines", HOT_LINES);
    recorder.parameter("edited_lines", EDITED_LINES);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_blames(
        &context.scratch.join("history"),
        &checkpoints,
        &mut recorder,
    ))?;

    let hot_changes = LARGEST.div_ceil(HOT_EVERY) as f64;
    let cold_changes = (LARGEST / FILES) as f64;
    for (name, changes) in [("hot", hot_changes), ("cold", cold_changes)] {
        recorder.at_most(
            &format!("n{LARGEST}.{name}.reads"),
            (READS_PER_CHANGE + READS_PER_ORIGIN) * changes + 10.0,
            "count",
        );
        recorder.at_most(&format!("n{LARGEST}.{name}.wall"), PAGE_BUDGET_SECONDS, "s");
    }
    Ok(recorder.finish())
}

/// The hot file as the test wrote it: each line and the change that wrote it.
struct HotFile {
    lines: Vec<(String, usize)>,
    bytes: Vec<u8>,
}

impl HotFile {
    fn new() -> Self {
        let mut file = Self {
            lines: (0..HOT_LINES)
                .map(|line| (format!("    let line_{line} = {line}; // change 0\n"), 0))
                .collect(),
            bytes: Vec::new(),
        };
        file.render();
        file
    }

    /// Change `index` rewrites [`EDITED_LINES`] lines.
    fn edit(&mut self, index: usize) {
        for edit in 0..EDITED_LINES {
            let line =
                (split_mix((index * EDITED_LINES + edit) as u64) % HOT_LINES as u64) as usize;
            self.lines[line] = (
                format!("    let line_{line} = {index}; // change {index}\n"),
                index,
            );
        }
        self.render();
    }

    fn render(&mut self) {
        self.bytes.clear();
        for (line, _) in &self.lines {
            self.bytes.extend_from_slice(line.as_bytes());
        }
    }
}

fn split_mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

async fn measure_blames(root: &Path, checkpoints: &[usize], recorder: &mut Recorder) -> Result<()> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut hot = HotFile::new();
    let mut cold_writer = 0;
    let mut effective = base;
    for index in 0..largest {
        let (path, expected, new) = if index % HOT_EVERY == 0 {
            let expected = (index > 0).then(|| hot.bytes.clone());
            if index > 0 {
                hot.edit(index);
            }
            (HOT.to_owned(), expected, hot.bytes.clone())
        } else {
            let file = index % FILES;
            let previous = index
                .checked_sub(FILES)
                .map(|previous| value(previous).into_bytes());
            if file == COLD {
                cold_writer = index;
            }
            (file_path(file), previous, value(index).into_bytes())
        };
        (head, effective) = write_at(
            &mutator,
            &scratch.content,
            (head, base, effective),
            index,
            &path,
            expected.as_deref(),
            &new,
        )
        .await?;
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let index_file = root.join(format!("paths-{size}.sqlite"));
        let paths = PathHistoryIndex::open(&index_file)?;
        let reader = RepositoryReader::new(&objects, &scratch.content);
        paths
            .catch_up(&reader, &reader.view(effective).await?)
            .await?;

        let browser = RepositoryBrowser::open(&objects, &scratch.content, head).await?;
        let limits = BlameLimits {
            max_bytes: MAX_BYTES,
            max_changes: NonZeroUsize::new(MAX_CHANGES).expect("a blame walks changes"),
        };
        let cold_lines = [(value(cold_writer), cold_writer)];
        for (name, path, expected) in [
            ("hot", HOT.to_owned(), hot.lines.as_slice()),
            ("cold", file_path(COLD), cold_lines.as_slice()),
        ] {
            let step = format!("n{size}.{name}");
            objects.take();
            let mut walls = Vec::with_capacity(SAMPLES);
            let mut reads = 0;
            let mut origins = 0;
            for sample in 0..SAMPLES {
                let started = Instant::now();
                let blame = browser
                    .blame(&paths, effective, &path, limits)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("{path} is missing"))?;
                walls.push(started.elapsed().as_secs_f64());
                if sample == 0 {
                    reads = objects.take().0;
                    origins = blame.origins.len();
                    ensure!(
                        blame.lines as usize == expected.len(),
                        "{path} has {} lines, not {}",
                        blame.lines,
                        expected.len()
                    );
                    for range in &blame.ranges {
                        let BlameOrigin::Change { index } = range.origin else {
                            anyhow::bail!("{path} has lines the blame left unresolved");
                        };
                        let change = blame.origins[index].change;
                        for line in range.start..range.start + range.len {
                            let writer = expected[line as usize].1;
                            ensure!(
                                change == change_id(writer),
                                "{path} line {line} is change {writer}'s"
                            );
                        }
                    }
                }
            }
            objects.take();
            recorder.wall(&step, measure::median(walls));
            recorder.value(&format!("{step}.reads"), reads as f64, "count");
            recorder.value(&format!("{step}.origins"), origins as f64, "count");
        }
        drop(paths);
        fs::remove_file(&index_file)?;
    }
    Ok(())
}

/// File `file` of the cold ones.
fn file_path(file: usize) -> String {
    format!("d{:02}/f{:02}.txt", file % 100, file / 100)
}

/// What change `index` writes to a cold file.
fn value(index: usize) -> String {
    format!("change {index}\n")
}
