//! Joining revisions of one change that went their own ways, as `wheat integrate` does,
//! and validating the combined revision it authors.
//!
//! `line`: the change is revised [`LINES`] times in a row, then twice more on two
//! sides, and the two sides are joined beside the line's tip. The merge reads the
//! revisions from the inputs down to where they meet, not the line below it. What
//! joining and validating the join still read more after a long line comes from the
//! repository's collections, which every revision adds a view and an application to,
//! deepening with the logarithm of their size; walking the line would read at least
//! one revision per revision. So the reads added from the shortest line to the longest,
//! per revision the line grew by, are at most [`READS_PER_LINE_REVISION`].
//!
//! `crisscross`: each round combines the two sides' revisions twice, differently, and
//! revises each combination, so the next join has two best bases, which merge into a
//! virtual base first, which has two of its own: one level per round. That work grows
//! with the rounds, bounded by `MergeBudget::max_recursion_depth`; it is recorded to
//! see the slope, without a contract.

use std::{fs, path::Path, time::Instant};

use anyhow::{Context, Result, ensure};
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, FileKind,
    FileValue, RevisionEdits, RevisionId, RevisionMetadata, Timestamp, ViewId,
};
use wheat_store::{
    ContentStore, IncrementBudget, LocalPublicationStore, PersistedRepositoryMutator,
    PublicationStore, RepositoryHead, RepositoryReader, RevisionMapStore,
};

use super::{
    CaseContext, CaseOutput, Recorder,
    history::{Counting, Scratch},
};

pub(crate) const NAME: &str = "revision_merge";
const LINES: [usize; 3] = [10, 100, 1_000];
const ROUNDS: [usize; 2] = [4, 12];
/// Reads added per revision the line grew by. Measured 2026-10-22 (release): joining 51
/// reads after 10 and 100 revisions and 67 after 1,000 (0.016 per revision);
/// validating 93, 94 and 118 (0.025).
const READS_PER_LINE_REVISION: f64 = 0.1;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let lines = context.scale.pick(LINES.to_vec(), vec![10, 30]);
    let rounds = context.scale.pick(ROUNDS.to_vec(), vec![2, 3]);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("lines", &lines);
    recorder.parameter("rounds", &rounds);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut measured = Vec::with_capacity(lines.len());
    for line in &lines {
        let root = context.scratch.join(format!("line{line}"));
        measured.push(runtime.block_on(measure_line(&root, *line, &mut recorder))?);
    }
    for round in &rounds {
        let root = context.scratch.join(format!("crisscross{round}"));
        runtime.block_on(measure_crisscross(&root, *round, &mut recorder))?;
    }
    if let (Some(first), Some(last)) = (measured.first(), measured.last())
        && lines.last() == LINES.last()
    {
        let grown = (lines[lines.len() - 1] - lines[0]) as f64;
        for (name, added) in [
            (
                "line.integrate.reads_per_revision",
                last.0 as f64 - first.0 as f64,
            ),
            (
                "line.validate.reads_per_revision",
                last.1 as f64 - first.1 as f64,
            ),
        ] {
            recorder.value(name, added / grown, "count");
            recorder.at_most(name, READS_PER_LINE_REVISION, "count");
        }
    }
    Ok(recorder.finish())
}

/// Revises the change `line` times, adding and removing one edit in turn so every
/// revision stays small, then joins two revisions of the tip beside it. Returns the
/// reads of the join and of validating it.
async fn measure_line(root: &Path, line: usize, recorder: &mut Recorder) -> Result<(u64, u64)> {
    let (change, mut tip) = Change::create(root).await?;
    for index in 0..line {
        let step = if index % 2 == 0 {
            Step::Add("t")
        } else {
            Step::Remove("t")
        };
        tip = change.revise(tip, step, i64::try_from(index)?).await?;
    }
    let left = change.revise(tip, Step::Add("left"), 1_000_000).await?;
    let right = change.revise(tip, Step::Add("right"), 2_000_000).await?;
    let step = format!("line{line}");
    let (reads, validate_reads) = change
        .measure_join(&[tip, left, right], &step, recorder)
        .await?;
    eprintln!("{NAME}: line of {line} revisions joined");
    Ok((reads, validate_reads))
}

/// Builds `rounds` rounds of criss-crossed combinations and joins the last two sides.
async fn measure_crisscross(root: &Path, rounds: usize, recorder: &mut Recorder) -> Result<()> {
    let (change, first) = Change::create(root).await?;
    let mut left = change.revise(first, Step::Add("a"), 100).await?;
    let mut right = change.revise(first, Step::Add("b"), 200).await?;
    let mut at = 300;
    for round in 0..rounds {
        let third = change
            .revise(first, Step::Add(&format!("c{round}")), at)
            .await?;
        let one = change.integrate(&[left, right]).await?;
        let other = change.integrate(&[left, right, third]).await?;
        left = change
            .revise(one, Step::Add(&format!("l{round}")), at + 1)
            .await?;
        right = change
            .revise(other, Step::Add(&format!("r{round}")), at + 2)
            .await?;
        at += 3;
    }
    change
        .measure_join(&[left, right], &format!("crisscross{rounds}"), recorder)
        .await?;
    eprintln!("{NAME}: {rounds} criss-crossed rounds joined");
    Ok(())
}

/// How a revision changes its predecessor's edits.
enum Step<'a> {
    /// Keeps them and adds one writing `path`.
    Add(&'a str),
    /// Keeps them except the one writing `path`.
    Remove(&'a str),
}

/// One change in a scratch repository, revised in process.
struct Change {
    objects: Counting,
    content: ContentStore,
    heads: LocalPublicationStore,
    base: ViewId,
    id: ChangeId,
}

impl Change {
    /// A repository holding one change, which writes `s`, and the view it makes.
    async fn create(root: &Path) -> Result<(Self, ViewId)> {
        fs::create_dir_all(root)?;
        let scratch = Scratch::open(root)?;
        let (base, _) = scratch.initialize().await?;
        let change = Self {
            objects: Counting::new(scratch.objects),
            content: scratch.content,
            heads: scratch.heads,
            base,
            id: ChangeId::from_bytes([0x5a; 16]),
        };
        let edits = change.edits(None, Step::Add("s")).await?;
        let (prepared, outcome) = change
            .mutator()
            .create(
                change.head()?,
                base,
                base,
                ChangeProposal {
                    id: change.id,
                    metadata: ChangeMetadata::default(),
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata::default(),
                    edits,
                },
            )
            .await?;
        change.mutator().publish(prepared).await?;
        Ok((change, outcome.resulting_view))
    }

    fn mutator(&self) -> PersistedRepositoryMutator<'_, Counting, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    fn reader(&self) -> RepositoryReader<'_, Counting> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    fn head(&self) -> Result<RepositoryHead> {
        Ok(self.heads.read()?)
    }

    async fn revision_in(&self, view: ViewId) -> Result<RevisionId> {
        let view = self.reader().view(view).await?;
        RevisionMapStore::new(&self.objects)
            .get(view.revisions, self.id)
            .await?
            .context("the view holds the change")
    }

    /// `predecessor`'s edits changed by `step`, the content it writes stored.
    async fn edits(
        &self,
        predecessor: Option<RevisionId>,
        step: Step<'_>,
    ) -> Result<RevisionEdits> {
        let mut edits = RevisionEdits::default();
        if let Some(predecessor) = predecessor {
            for id in self.reader().revision(predecessor).await?.edits {
                let edit = self.reader().edit(id).await?;
                if matches!(step, Step::Remove(path) if edit.path == path) {
                    edits.transition.removed.insert(id);
                } else {
                    edits.edits.insert(id, edit);
                    edits.transition.preserved.insert(id);
                }
            }
        }
        if let Step::Add(path) = step {
            self.content.put(path.as_bytes())?;
            let edit = EditRecord::new(
                path,
                EditOperation::ExactFile {
                    expected: None,
                    replacement: Some(FileValue {
                        content: ContentId::of(path.as_bytes()),
                        kind: FileKind::Regular,
                    }),
                },
            )?;
            edits.transition.added.insert(edit.id());
            edits.edits.insert(edit.id(), edit);
        }
        Ok(edits)
    }

    /// Revises the change where `view` holds it, committed at `at`; returns the view.
    async fn revise(&self, view: ViewId, step: Step<'_>, at: i64) -> Result<ViewId> {
        let predecessor = self.revision_in(view).await?;
        let edits = self.edits(Some(predecessor), step).await?;
        let metadata = RevisionMetadata {
            committed_at: Timestamp {
                seconds: at,
                offset_minutes: 0,
            },
            ..RevisionMetadata::default()
        };
        let (prepared, outcome) = self
            .mutator()
            .revise(
                self.head()?,
                self.base,
                view,
                self.id,
                predecessor,
                metadata,
                edits,
            )
            .await?;
        self.mutator().publish(prepared).await?;
        Ok(outcome.resulting_view)
    }

    async fn integrate(&self, frontiers: &[ViewId]) -> Result<ViewId> {
        let (prepared, outcome) = self
            .mutator()
            .integrate(self.head()?, self.base, frontiers)
            .await?;
        self.mutator().publish(prepared).await?;
        Ok(outcome.resulting_view)
    }

    /// Joins `frontiers`, which must combine the change, and validates the join as an
    /// increment, recording the reads and time of both under `step`.
    async fn measure_join(
        &self,
        frontiers: &[ViewId],
        step: &str,
        recorder: &mut Recorder,
    ) -> Result<(u64, u64)> {
        let before = self.head()?;
        self.objects.take();
        let started = Instant::now();
        let (prepared, outcome) = self
            .mutator()
            .integrate(before, self.base, frontiers)
            .await?;
        let integrate_wall = started.elapsed().as_secs_f64();
        let (reads, _) = self.objects.take();
        ensure!(outcome.combined.len() == 1, "the join combines the change");
        let after = self.mutator().publish(prepared).await?;
        self.objects.take();
        let started = Instant::now();
        self.reader()
            .validate_increment(before.envelope, after.envelope, IncrementBudget::STANDARD)
            .await?;
        let validate_wall = started.elapsed().as_secs_f64();
        let (validate_reads, _) = self.objects.take();
        recorder.wall(&format!("{step}.integrate"), integrate_wall);
        recorder.value(&format!("{step}.integrate.reads"), reads as f64, "count");
        recorder.wall(&format!("{step}.validate"), validate_wall);
        recorder.value(
            &format!("{step}.validate.reads"),
            validate_reads as f64,
            "count",
        );
        Ok((reads, validate_reads))
    }
}
