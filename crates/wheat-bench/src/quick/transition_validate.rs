//! Transition validation against complete validation (formerly the ignored
//! `transition_bench::validate_transition_vs_validate_complete` test).
//!
//! `validate_transition` checks only what a publication changed, so it scales with the
//! change, not the repository. A repository of `baseline` changes gets `delta` more;
//! validating that transition must take under 1 s and at most half as long as
//! validating the whole repository.

use std::{path::Path, time::Instant};

use anyhow::Result;
use wheat_core::{
    ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    Repository, RepositoryStateId, RevisionEdits, Tree,
};
use wheat_store::{
    ContentStore, LocalPublicationStore, LooseObjectStore, RepositoryPublisher, RepositoryReader,
};

use super::{CaseContext, CaseOutput, Recorder};
use crate::measure;

pub(crate) const NAME: &str = "transition_validate";
const SAMPLES: usize = 3;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let baseline = context.scale.pick(500, 20);
    let delta = context.scale.pick(5, 2);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("baseline_changes", baseline);
    recorder.parameter("delta_changes", delta);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_validation(
        &context.scratch,
        baseline,
        delta,
        &mut recorder,
    ))?;
    Ok(recorder.finish())
}

async fn measure_validation(
    root: &Path,
    baseline: usize,
    delta: usize,
    recorder: &mut Recorder,
) -> Result<()> {
    let objects = LooseObjectStore::new(root.join("objects"));
    let content = ContentStore::new(root.join("content"));
    let heads = LocalPublicationStore::new(root.join("repository.head"));

    let started = Instant::now();
    let (baseline_state, delta_state) =
        build_repository(&objects, &content, &heads, baseline, delta).await?;
    recorder.wall("build", started.elapsed().as_secs_f64());

    let reader = RepositoryReader::new(&objects, &content);
    // One untimed run of each warms the OS caches.
    reader
        .validate_transition(baseline_state, delta_state)
        .await?;
    reader.validate_complete(delta_state).await?;

    let mut transition = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        reader
            .validate_transition(baseline_state, delta_state)
            .await?;
        transition.push(started.elapsed().as_secs_f64());
    }
    let mut complete = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        reader.validate_complete(delta_state).await?;
        complete.push(started.elapsed().as_secs_f64());
    }

    let transition = measure::median(transition);
    let complete = measure::median(complete);
    recorder.wall("transition", transition);
    recorder.wall("complete", complete);
    recorder.value("transition_share", transition / complete * 100.0, "percent");
    // The budget from the design doc: a small delta validates in under a second.
    recorder.at_most("transition.wall", 1.0, "s");
    // At least twice as fast as validating everything.
    recorder.at_most("transition_share", 50.0, "percent");
    Ok(())
}

fn edit(path: &str, value: &[u8]) -> Result<RevisionEdits> {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )?;
    let id = record.id();
    Ok(RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    })
}

/// Publishes `baseline` changes, then `delta` more; returns the two published states.
async fn build_repository(
    objects: &LooseObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    baseline: usize,
    delta: usize,
) -> Result<(RepositoryStateId, RepositoryStateId)> {
    for index in 0..baseline + delta {
        content.put(format!("value-{index}").as_bytes())?;
    }

    let mut repository = Repository::new(Tree::new());
    let basis = repository.base_view().id;
    let mut effective = basis;
    let mut add_changes =
        |repository: &mut Repository, range: std::ops::Range<usize>, directory| {
            for index in range {
                let (_, outcome) = repository.create_change(
                    basis,
                    effective,
                    [],
                    edit(
                        &format!("src/{directory}/{index:06}.txt"),
                        format!("value-{index}").as_bytes(),
                    )?,
                    ApplicationContext::new(content),
                )?;
                effective = outcome.resulting_view;
            }
            anyhow::Ok(())
        };

    add_changes(&mut repository, 0..baseline, "baseline")?;
    let publisher = RepositoryPublisher::new(objects, content, heads);
    let baseline_head = publisher.initialize(&repository).await?;
    let reader = RepositoryReader::new(objects, content);
    let baseline_state = reader.envelope(baseline_head.envelope).await?.state;

    add_changes(&mut repository, baseline..baseline + delta, "delta")?;
    let delta_head = publisher.publish(baseline_head, &repository).await?;
    let delta_state = reader.envelope(delta_head.envelope).await?.state;
    Ok((baseline_state, delta_state))
}
