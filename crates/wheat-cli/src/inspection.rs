//! Human-readable immutable edit and conflict evidence inspection.
use std::fmt::Write as _;

use anyhow::{Context, Result};
use wheat_core::{EditOperation, Repository, RevisionId};

pub(crate) fn edits(
    repository: &Repository,
    revision: RevisionId,
    output: &mut String,
) -> Result<()> {
    let revision = repository
        .revision(revision)
        .context("revision is not retained")?;
    for (id, edit) in revision.edits() {
        write!(output, "{id} {} ", edit.path)?;
        match &edit.operation {
            EditOperation::GuardedText {
                kind,
                before,
                expected,
                replacement,
                after,
                start_anchor,
                end_anchor,
            } => writeln!(
                output,
                "guarded-text kind={kind:?} before={before} expected={expected} replacement={replacement} after={after} start-anchor={start_anchor} end-anchor={end_anchor}"
            )?,
            EditOperation::ExactFile {
                expected,
                replacement,
            } => writeln!(
                output,
                "exact-file expected={expected:?} replacement={replacement:?}"
            )?,
        }
    }
    let transition = revision.edit_transition();
    writeln!(
        output,
        "transition preserved={} added={} removed={} subsumed={}",
        transition.preserved.len(),
        transition.added.len(),
        transition.removed.len(),
        transition.subsumed.len()
    )?;
    write_ids(output, "preserved", &transition.preserved)?;
    write_ids(output, "added", &transition.added)?;
    write_ids(output, "removed", &transition.removed)?;
    for subsumption in &transition.subsumed {
        write!(output, "subsumed {} ->", subsumption.source)?;
        for replacement in &subsumption.replacements {
            write!(output, " {replacement}")?;
        }
        writeln!(output)?;
    }
    Ok(())
}

fn write_ids<T: std::fmt::Display>(
    output: &mut String,
    name: &str,
    ids: &std::collections::BTreeSet<T>,
) -> Result<()> {
    write!(output, "{name}")?;
    for id in ids {
        write!(output, " {id}")?;
    }
    writeln!(output)?;
    Ok(())
}

pub(crate) fn conflicts(repository: &Repository, output: &mut String) -> Result<()> {
    let working = repository.working_view().id;
    for conflict in repository.conflicts_in_view(working)? {
        writeln!(output, "conflict {} on {}", conflict.id, conflict.base_view)?;
        for (edit, evidence) in &conflict.edits {
            writeln!(
                output,
                "  edit {edit} target={:?} reason={:?}",
                evidence.target, evidence.reason
            )?;
        }
    }
    Ok(())
}
