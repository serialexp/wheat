use super::*;

pub(super) fn execute<C: ContentIo + ?Sized>(
    revision: RevisionId,
    target: &RepositoryView,
    edits: &BTreeMap<EditId, EditRecord>,
    context: ApplicationContext<'_, C>,
) -> Result<Application, RepositoryError> {
    let output = crate::edit_application::apply_edits(
        &target.tree,
        edits.iter().map(|(id, edit)| (*id, edit.clone())),
        |id| context.content.resolve(id),
        context.limits,
    )
    .map_err(|error| RepositoryError::EditApplication(format!("{error:?}")))?;

    // The application engine returns generated objects in canonical ID order. Durability must
    // precede insertion of any repository metadata which references them.
    for (id, bytes) in &output.generated_content {
        if crate::ContentId::of(bytes) != *id {
            return Err(RepositoryError::InvalidRepositoryState(
                "generated content identity",
            ));
        }
        context
            .content
            .store_generated(*id, bytes)
            .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
    }

    let mut applied = BTreeMap::new();
    let mut failed = BTreeMap::new();
    for (id, outcome) in output.outcomes {
        let record = edits
            .get(&id)
            .expect("application only reports selected edits");
        let target_value = target.tree.get(&record.path).copied();
        match outcome {
            crate::edit_application::ApplicationOutcome::Applied { read, write } => {
                applied.insert(
                    id,
                    AppliedEditEvidence {
                        target: target_value,
                        read: read.map(byte_range),
                        write: write.map(byte_range),
                    },
                );
            }
            outcome => {
                let reason = match outcome {
                    crate::edit_application::ApplicationOutcome::NoMatch => EditFailure::NoMatch,
                    crate::edit_application::ApplicationOutcome::Ambiguous => {
                        EditFailure::Ambiguous
                    }
                    crate::edit_application::ApplicationOutcome::Interference { with } => {
                        EditFailure::Interference {
                            with: with.into_iter().collect(),
                        }
                    }
                    crate::edit_application::ApplicationOutcome::KindMismatch { .. } => {
                        EditFailure::KindMismatch
                    }
                    crate::edit_application::ApplicationOutcome::ExpectedFileMismatch {
                        ..
                    } => EditFailure::ExpectedFileMismatch,
                    crate::edit_application::ApplicationOutcome::Applied { .. } => unreachable!(),
                };
                failed.insert(
                    id,
                    FailedEditEvidence {
                        target: target_value,
                        reason,
                    },
                );
            }
        }
    }
    let result = if failed.is_empty() {
        ApplicationResult::Clean { tree: output.tree }
    } else {
        ApplicationResult::Conflicted {
            partial_tree: output.tree,
            conflict: Conflict::new(target.id, failed),
        }
    };
    Ok(Application::new(revision, target.id, result, applied))
}

fn byte_range(range: std::ops::Range<usize>) -> ByteRange {
    ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}
