//! Git projection of changes with many checkpoints.
//!
//! `wheat push` writes one commit per change, and each commit message folds the change's
//! title, description, and the note of every checkpoint of that change. A stack of
//! `changes` changes gets `checkpoints` revisions each, with a note on the first and on
//! every tenth. Composing the messages must read each checkpoint once in total, so the
//! projection grows with the number of revisions and stays within its budget.

use std::{collections::BTreeSet, path::Path, time::Instant};

use anyhow::{Result, ensure};
use wheat_core::{
    Actor, ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditId, EditOperation, EditRecord,
    EditTransition, FileKind, FileValue, Repository, RevisionEdits, RevisionId, RevisionMetadata,
    Tree, ViewId,
};
use wheat_git::export::{ExportOptions, export_revision_history};
use wheat_store::{
    ContentStore, Durability, LocalPublicationStore, PackedObjectStore, PersistedRepositoryMutator,
    RepositoryHead, RepositoryPublisher, RepositoryReader,
};

use super::{CaseContext, CaseOutput, Recorder};
use crate::measure;

pub(crate) const NAME: &str = "git_export_notes";
const SAMPLES: usize = 3;
/// Budget for projecting the full-size stack into a fresh bare repository. Measured on
/// 2026-09-26 (release): 0.03 s at full size, 0.08 s for 4,000 revisions in 100 changes.
const EXPORT_BUDGET_SECONDS: f64 = 0.5;
/// A checkpoint note on every this-many checkpoints after the first.
const NOTE_EVERY: usize = 10;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    // Building the history costs far more than exporting it: every mutation walks all
    // views since the basis (see TODO.md, "open walks every view since the basis"), so
    // the build grows with the square of the revision count. 2,000 revisions keep it
    // near a minute.
    let changes = context.scale.pick(50, 5);
    let checkpoints = context.scale.pick(40, 12);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("checkpoints_per_change", checkpoints);
    recorder.parameter("samples", SAMPLES);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(measure_export(
        &context.scratch,
        changes,
        checkpoints,
        &mut recorder,
    ))?;
    Ok(recorder.finish())
}

async fn measure_export(
    root: &Path,
    changes: usize,
    checkpoints: usize,
    recorder: &mut Recorder,
) -> Result<()> {
    // Building thousands of revisions with a sync per publication would dwarf
    // the export being measured, so the scratch repository defers its syncs.
    let objects = PackedObjectStore::open_deferred(root.join("objects"))?;
    let content = ContentStore::with_durability(root.join("content"), Durability::Deferred);
    let heads =
        LocalPublicationStore::with_durability(root.join("repository.head"), Durability::Deferred);

    let started = Instant::now();
    let (base, selected, head) =
        build_repository(&objects, &content, &heads, changes, checkpoints).await?;
    recorder.wall("build", started.elapsed().as_secs_f64());

    let reader = RepositoryReader::new(&objects, &content);
    let state = reader
        .state(reader.envelope(head.envelope).await?.state)
        .await?;

    // Sample 0 is an untimed run that warms the OS caches.
    let mut walls = Vec::with_capacity(SAMPLES);
    for sample in 0..=SAMPLES {
        let git = gix::init_bare(root.join(format!("git-{sample}")))?;
        let started = Instant::now();
        let exported = export_revision_history(
            &objects,
            &content,
            selected,
            base,
            state.changes,
            &git,
            &ExportOptions::default(),
        )
        .await?;
        let elapsed = started.elapsed().as_secs_f64();
        if sample > 0 {
            walls.push(elapsed);
        }
        ensure!(
            exported.projected.len() == changes,
            "projected {} commits for {changes} changes",
            exported.projected.len()
        );
        let tip = git.find_commit(exported.tip.expect("changes were projected"))?;
        let message = String::from_utf8_lossy(tip.message_raw()?).into_owned();
        let last = changes - 1;
        ensure!(
            message.starts_with(&format!("Change {last}\n\nWhy change {last} exists.")),
            "unexpected tip message {message:?}"
        );
        if checkpoints > NOTE_EVERY {
            ensure!(
                message.contains(&format!("Checkpoint {NOTE_EVERY} of change {last}")),
                "tip message lost a checkpoint note: {message:?}"
            );
        }
    }
    recorder.wall("export", measure::median(walls));
    recorder.at_most("export.wall", EXPORT_BUDGET_SECONDS, "s");
    Ok(())
}

fn rewrite(path: &str, value: &[u8], previous: Option<EditId>) -> Result<(EditId, RevisionEdits)> {
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
    Ok((
        id,
        RevisionEdits {
            edits: [(id, record)].into(),
            transition: EditTransition {
                added: [id].into(),
                removed: previous.into_iter().collect(),
                ..EditTransition::default()
            },
        },
    ))
}

/// Builds the stack one published revision at a time, the way `wheat change new` and
/// `wheat add` do; returns the base view, the selected view, and the published head.
async fn build_repository(
    objects: &PackedObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    changes: usize,
    checkpoints: usize,
) -> Result<(ViewId, ViewId, RepositoryHead)> {
    let author = Actor {
        name: "Wheat Bench".into(),
        email: "wheat-bench@example.invalid".into(),
    };
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut head = RepositoryPublisher::new(objects, content, heads)
        .initialize(&repository)
        .await?;
    let mutator = PersistedRepositoryMutator::new(objects, content, heads);
    let mut effective = base;
    let mut below: Option<ChangeId> = None;
    for index in 0..changes {
        let change = ChangeId::new();
        let path = format!("src/f{index:05}.txt");
        let mut previous: Option<(RevisionId, EditId)> = None;
        for checkpoint in 0..checkpoints {
            let value = format!("change {index}\ncheckpoint {checkpoint}\n");
            content.put(value.as_bytes())?;
            let (edit, edits) = rewrite(&path, value.as_bytes(), previous.map(|(_, e)| e))?;
            let message = if checkpoint == 0 {
                format!("Change {index}\n\nWhy change {index} exists.")
            } else if checkpoint % NOTE_EVERY == 0 {
                format!("Checkpoint {checkpoint} of change {index}")
            } else {
                String::new()
            };
            let metadata = RevisionMetadata {
                message,
                author: author.clone(),
                committer: author.clone(),
                ..RevisionMetadata::default()
            };
            let (prepared, outcome) = match previous {
                None => {
                    mutator
                        .create(
                            head,
                            base,
                            effective,
                            ChangeProposal {
                                id: change,
                                metadata: ChangeMetadata {
                                    title: format!("Change {index}"),
                                    creator: author.clone(),
                                    ..ChangeMetadata::default()
                                },
                                dependencies: below.into_iter().collect::<BTreeSet<_>>(),
                                revision_metadata: metadata,
                                edits,
                            },
                        )
                        .await?
                }
                Some((revision, _)) => {
                    mutator
                        .revise(head, base, effective, change, revision, metadata, edits)
                        .await?
                }
            };
            head = mutator.publish(prepared).await?;
            effective = outcome.resulting_view;
            previous = Some((outcome.revision_id, edit));
        }
        below = Some(change);
    }
    Ok((base, effective, head))
}
