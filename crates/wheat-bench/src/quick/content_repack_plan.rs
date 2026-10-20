//! Planning and preparing a content repack of many content IDs (formerly the ignored
//! `content_repack_scale::content_repack_planner_scale` test).
//!
//! Writes `ids` small content objects in files of 20 versions each (one tip plus 19
//! history pairs that delta well), then plans and prepares one generation of them.
//! Each phase gets its own wall time and peak RSS. Measured only: the planner's
//! `ContentRepackLimits::max_content_ids` bound is chosen from these numbers.

use std::time::Instant;

use anyhow::{Result, bail};
use wheat_core::VerifiedContent;
use wheat_store::{
    ContentHistoryPair, ContentRepackInput, ContentRepackLimits, ContentRepackPlan, ContentStore,
    Durability, plan_content_repack, prepare_content_repack,
};

use super::{CaseContext, CaseOutput, Recorder};
use crate::measure::PhasePeak;

pub(crate) const NAME: &str = "content_repack_plan";
/// Versions per synthetic file.
const VERSIONS_PER_FILE: usize = 20;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let ids = context.scale.pick(250_000, 2_000);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("content_ids", ids);
    recorder.parameter("versions_per_file", VERSIONS_PER_FILE);

    let store =
        ContentStore::with_durability(context.scratch.join("content"), Durability::Deferred);
    let mut peak = PhasePeak::start()?;

    let started = Instant::now();
    let mut content_ids = Vec::with_capacity(ids);
    let mut tips = Vec::with_capacity(ids.div_ceil(VERSIONS_PER_FILE));
    let mut history = Vec::with_capacity(ids);
    let mut body = Vec::with_capacity(512);
    for file in 0..ids.div_ceil(VERSIONS_PER_FILE) {
        let versions = VERSIONS_PER_FILE.min(ids - file * VERSIONS_PER_FILE);
        let mut batch = Vec::with_capacity(versions);
        for version in 0..versions {
            write_body(&mut body, file, version);
            batch.push(VerifiedContent::new(body.clone()));
        }
        for pair in batch.windows(2) {
            history.push(ContentHistoryPair {
                older: pair[0].id(),
                newer: pair[1].id(),
            });
        }
        tips.extend(batch.last().map(VerifiedContent::id));
        content_ids.extend(batch.iter().map(VerifiedContent::id));
        let batch = batch
            .iter()
            .map(VerifiedContent::borrowed)
            .collect::<Vec<_>>();
        store.put_many(&batch, 1)?;
    }
    recorder.wall("write", started.elapsed().as_secs_f64());
    recorder.peak_rss("write", peak.take()?);
    recorder.value("history_pairs", history.len() as f64, "count");

    let limits = ContentRepackLimits {
        max_content_ids: usize::MAX,
        ..ContentRepackLimits::default()
    };
    let started = Instant::now();
    let plan = plan_content_repack(
        &store,
        None,
        ContentRepackInput {
            content_ids,
            tips,
            history,
        },
        limits,
    )?;
    recorder.wall("plan", started.elapsed().as_secs_f64());
    recorder.peak_rss("plan", peak.take()?);
    let ContentRepackPlan::Replace(analysis) = plan else {
        bail!("planning a fresh generation returned no work");
    };
    recorder.value(
        "estimated_bytes",
        analysis.estimated_bytes() as f64,
        "bytes",
    );

    let started = Instant::now();
    let prepared = prepare_content_repack(&store, analysis)?;
    recorder.wall("prepare", started.elapsed().as_secs_f64());
    recorder.peak_rss("prepare", peak.take()?);
    recorder.value("entries", prepared.manifest.entries.len() as f64, "count");
    recorder.value("scratch_bytes", prepared.scratch_bytes() as f64, "bytes");
    Ok(recorder.finish())
}

/// Version `version` of file `file`: a few hundred bytes that differ from the
/// neighbouring version in one line, so each history pair deltas well.
fn write_body(bytes: &mut Vec<u8>, file: usize, version: usize) {
    use std::io::Write as _;
    bytes.clear();
    for line in 0..16 {
        let stamp = if line == version % 16 { version } else { 0 };
        writeln!(bytes, "file {file} line {line} stamp {stamp}").expect("writing to a Vec");
    }
}
