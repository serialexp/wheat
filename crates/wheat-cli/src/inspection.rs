//! Human-readable immutable edit and conflict evidence inspection.
use std::fmt::Write as _;

use anyhow::Result;
use wheat_core::{EditOperation, EditRecord, RevisionRecord};

pub(crate) fn edits(
    revision: &RevisionRecord,
    edits: &[(wheat_core::EditId, EditRecord)],
    output: &mut String,
) -> Result<()> {
    for (id, edit) in edits {
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
    let transition = &revision.transition;
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
