//! Paging a served repository's history newest first, as a host's history page does,
//! in canonical order and by commit time.
//!
//! `wheat_store::RepositoryBrowser::history` finds a page's first change through the
//! view's application skip chain (O(log n) reads), then follows each application's
//! `previous`, reading its application, revision and change and looking the change
//! up in the change map. `history_by_time` finds it through the view's history map
//! (O(log n) node reads) and reads the same objects per change. So a page costs about
//! the same at any depth and any history size. Changes are created on the frontier
//! and published one at a time, as in `client_history`, each committed a minute
//! after the one before. At each checkpoint the newest page and a page from the
//! middle of the history are read [`SAMPLES`] times each in both orders, recording
//! the objects the first read takes and the median wall time. At [`LARGEST`] changes
//! a page of [`PAGE`] may read at most [`READS_PER_PAGE`] objects and take
//! [`PAGE_BUDGET_SECONDS`].

use std::{fs, num::NonZeroUsize, path::Path, time::Instant};

use anyhow::Result;
use wheat_core::HistoryKey;
use wheat_store::{LocalPublicationStore, PersistedRepositoryMutator, RepositoryBrowser};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Layout, Scratch, change_id, committed_at, create_at},
};
use crate::measure;

pub(crate) const NAME: &str = "browse_history";
const SAMPLES: usize = 5;
/// Changes per page.
const PAGE: usize = 50;
/// The history size the budgets are checked at.
const LARGEST: usize = 20_000;
const PAGE_BUDGET_SECONDS: f64 = 0.05;
/// About five reads per change, plus the skip chain and the view check.
const READS_PER_PAGE: f64 = (PAGE * 6 + 40) as f64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let checkpoints = context
        .scale
        .pick(vec![1_000, 5_000, LARGEST], vec![100, 300]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("checkpoints", &checkpoints);
    recorder.parameter("page", PAGE);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_pages(
        &context.scratch.join("history"),
        &checkpoints,
        &mut recorder,
    ))?;

    for page in ["newest", "middle", "newest_by_time", "middle_by_time"] {
        let step = format!("n{LARGEST}.{page}");
        recorder.at_most(&format!("{step}.wall"), PAGE_BUDGET_SECONDS, "s");
        recorder.at_most(&format!("{step}.reads"), READS_PER_PAGE, "count");
    }
    Ok(recorder.finish())
}

async fn measure_pages(root: &Path, checkpoints: &[usize], recorder: &mut Recorder) -> Result<()> {
    fs::create_dir_all(root)?;
    let scratch = Scratch::open(root)?;
    let (base, mut head) = scratch.initialize().await?;
    let objects = Counting::new(scratch.objects);
    let mutator = PersistedRepositoryMutator::<_, LocalPublicationStore>::new(
        &objects,
        &scratch.content,
        &scratch.heads,
    );
    let page = NonZeroUsize::new(PAGE).expect("a page holds changes");
    let largest = checkpoints.iter().copied().max().unwrap_or(0);
    let mut effective = base;
    for index in 0..largest {
        (head, effective) = create_at(
            &mutator,
            &scratch.content,
            head,
            base,
            effective,
            index,
            Layout::Spread,
        )
        .await?;
        let size = index + 1;
        if !checkpoints.contains(&size) {
            continue;
        }
        let browser = RepositoryBrowser::open(&objects, &scratch.content, head).await?;
        for (name, before) in [("newest", None), ("middle", Some(size as u64 / 2))] {
            let step = format!("n{size}.{name}");
            objects.take();
            let mut walls = Vec::with_capacity(SAMPLES);
            let mut reads = 0;
            for sample in 0..SAMPLES {
                let started = Instant::now();
                let history = browser.history(effective, before, page).await?;
                walls.push(started.elapsed().as_secs_f64());
                anyhow::ensure!(
                    history.items.len() == PAGE.min(before.map_or(size, |at| at as usize)),
                    "the {name} page holds {} changes",
                    history.items.len()
                );
                if sample == 0 {
                    reads = objects.take().0;
                }
            }
            objects.take();
            recorder.wall(&step, measure::median(walls));
            recorder.value(&format!("{step}.reads"), reads as f64, "count");
        }
        // The middle page by time starts at the change made halfway, which is also
        // the one committed halfway.
        let middle = size / 2;
        let middle_key = HistoryKey::new(committed_at(middle).seconds, change_id(middle));
        for (name, from) in [
            ("newest_by_time", None),
            ("middle_by_time", Some(middle_key)),
        ] {
            let step = format!("n{size}.{name}");
            objects.take();
            let mut walls = Vec::with_capacity(SAMPLES);
            let mut reads = 0;
            for sample in 0..SAMPLES {
                let started = Instant::now();
                let history = browser.history_by_time(effective, from, page).await?;
                walls.push(started.elapsed().as_secs_f64());
                let expected = PAGE.min(from.map_or(size, |_| middle + 1));
                anyhow::ensure!(
                    history.items.len() == expected,
                    "the {name} page holds {} changes, not {expected}",
                    history.items.len()
                );
                if sample == 0 {
                    reads = objects.take().0;
                }
            }
            objects.take();
            recorder.wall(&step, measure::median(walls));
            recorder.value(&format!("{step}.reads"), reads as f64, "count");
        }
    }
    Ok(())
}
