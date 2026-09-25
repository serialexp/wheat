//! The edits later changes made to a few paths, for placing an edit into an earlier change.
//!
//! A change's spot is the view after every change before it in canonical order, so the file the
//! author sees is the change's result with every later change's edits on top. This walks those
//! later changes and returns, for each selected path, the edits that wrote it and where.

use std::collections::{BTreeMap, BTreeSet};

use wheat_core::{
    ApplicationResultRecord, AppliedEditEvidence, ChangeId, EditId, EditOperation, EditRecord,
    FailedEditEvidence, FileValue, RepositoryError, RevisionId, TreeId,
    edit_placement::{LaterStep, LaterWrite},
};

use crate::{
    ApplicationMapStore, AuthoringSession, ContentStore, ImmutableObjectStore, MerkleTreeStore,
    PersistedRepositoryMutator, PublicationStore, RepositoryMutationError, RepositoryReader,
};

type Result<T> = std::result::Result<T, RepositoryMutationError>;

/// What later changes did to each selected path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaterEdits {
    pub paths: BTreeMap<String, PathHistory>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathHistory {
    /// The path in the revised change's result.
    pub below: Option<FileValue>,
    /// Each later change with an edit on the path, in canonical order.
    pub steps: Vec<PathStep>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStep {
    pub change: ChangeId,
    pub revision: RevisionId,
    /// The change's edits on this path.
    pub edits: BTreeMap<EditId, EditRecord>,
    /// Evidence for the edits that applied; the others failed.
    pub applied: BTreeMap<EditId, AppliedEditEvidence>,
    /// The path once this change applied.
    pub after: Option<FileValue>,
}

impl PathHistory {
    /// The history up to the last step that leaves the path at `shown`, or `None` when no step
    /// does.
    ///
    /// A workspace materialized before some later changes existed shows the path as it was
    /// after only the earlier of them. Its author edited that version, so only those steps'
    /// edits can have been seen and moved; the later steps are simply reapplied. A workspace
    /// that never had the path saw none of them.
    pub fn up_to(&self, shown: Option<FileValue>) -> Option<Self> {
        let seen = match self.steps.iter().rposition(|step| step.after == shown) {
            Some(last) => last + 1,
            None if self.below == shown || shown.is_none() => 0,
            None => return None,
        };
        Some(Self {
            below: self.below,
            steps: self.steps[..seen].to_vec(),
        })
    }

    /// The later writes in the form [`wheat_core::edit_placement::attribute`] follows.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn placement_steps(&self, content: &ContentStore) -> Result<Vec<LaterStep>> {
        let length = |value: FileValue| -> Result<usize> {
            let length = content
                .logical_length(value.content)
                .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
            usize::try_from(length).map_err(|_| {
                RepositoryError::InvalidRepositoryState("content length exceeds memory").into()
            })
        };
        let mut steps = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            let mut writes = Vec::new();
            for (id, evidence) in &step.applied {
                match &step.edits[id].operation {
                    EditOperation::GuardedText { replacement, .. } => {
                        // An edit whose replacement was already present wrote nothing.
                        let Some(write) = evidence.write else {
                            continue;
                        };
                        let range = |at: u64| {
                            usize::try_from(at).map_err(|_| {
                                RepositoryError::InvalidRepositoryState(
                                    "edit evidence exceeds memory",
                                )
                            })
                        };
                        let replacement = content
                            .logical_length(*replacement)
                            .map_err(|error| RepositoryError::ContentIo(error.to_string()))?;
                        writes.push(LaterWrite::Range {
                            edit: *id,
                            write: range(write.start)?..range(write.end)?,
                            replacement_len: range(replacement)?,
                        });
                    }
                    EditOperation::ExactFile { replacement, .. } => {
                        if evidence.target == *replacement {
                            continue;
                        }
                        writes.push(LaterWrite::WholeFile {
                            edit: *id,
                            len: replacement.map(length).transpose()?,
                        });
                    }
                }
            }
            if !writes.is_empty() {
                steps.push(LaterStep {
                    change: step.change,
                    writes,
                });
            }
        }
        Ok(steps)
    }
}

impl<O: ImmutableObjectStore, P: PublicationStore> PersistedRepositoryMutator<'_, O, P> {
    /// The edits every change after `change` in canonical order made to `paths`.
    ///
    /// Only the later changes' applications are read, and an edit record only when its recorded
    /// target matches the current value of a selected path, so the cost follows the changes
    /// above `change` and the edits that could touch the paths. With `change` on top it reads
    /// nothing beyond the change's own result.
    #[tracing::instrument(level = "debug", skip_all, fields(paths = paths.len()))]
    pub async fn later_edits(
        &self,
        session: &AuthoringSession,
        change: ChangeId,
        paths: &BTreeSet<String>,
    ) -> Result<LaterEdits> {
        let active = &session.active;
        let order = &active.persisted_order;
        let position = order
            .iter()
            .position(|id| *id == change)
            .ok_or(RepositoryError::UnknownChange(change))?;
        let reader = RepositoryReader::new(self.objects, self.content);
        let applications = ApplicationMapStore::new(self.objects);
        let trees = MerkleTreeStore::new(self.objects);
        let sorted = paths.iter().map(String::as_str).collect::<Vec<_>>();

        let application = |change: ChangeId| {
            let applications = &applications;
            let reader = &reader;
            async move {
                let id = applications
                    .get(active.effective.applications, change)
                    .await?
                    .ok_or(RepositoryError::InvalidRepositoryState(
                        "effective view lacks canonical application",
                    ))?;
                Ok::<_, RepositoryMutationError>(reader.application(id).await?)
            }
        };
        let resulting_tree = |result: &ApplicationResultRecord| match result {
            ApplicationResultRecord::Clean { tree } => *tree,
            ApplicationResultRecord::Conflicted { partial_tree, .. } => *partial_tree,
        };
        let below: TreeId = resulting_tree(&application(change).await?.result);
        let mut history = paths
            .iter()
            .cloned()
            .zip(trees.lookup_many(below, &sorted).await?)
            .map(|(path, below)| {
                (
                    path,
                    PathHistory {
                        below,
                        steps: Vec::new(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut current = history
            .iter()
            .map(|(path, history)| (path.clone(), history.below))
            .collect::<BTreeMap<_, _>>();

        for later in &order[position + 1..] {
            let record = application(*later).await?;
            let values = current.values().copied().collect::<BTreeSet<_>>();
            let failed = match record.result {
                ApplicationResultRecord::Conflicted { conflict, .. } => {
                    reader.conflict(conflict).await?.edits
                }
                ApplicationResultRecord::Clean { .. } => {
                    BTreeMap::<EditId, FailedEditEvidence>::new()
                }
            };
            let candidates = record
                .applied
                .iter()
                .map(|(id, evidence)| (id, evidence.target))
                .chain(failed.iter().map(|(id, evidence)| (id, evidence.target)))
                .filter(|(_, target)| values.contains(target))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            let mut on_paths = BTreeMap::<String, BTreeMap<EditId, EditRecord>>::new();
            for id in candidates {
                let edit = reader.edit(id).await?;
                if current.contains_key(&edit.path) {
                    on_paths
                        .entry(edit.path.clone())
                        .or_default()
                        .insert(id, edit);
                }
            }
            if on_paths.is_empty() {
                continue;
            }
            let written = on_paths.keys().map(String::as_str).collect::<Vec<_>>();
            let after = trees
                .lookup_many(resulting_tree(&record.result), &written)
                .await?;
            for ((path, edits), value) in on_paths.into_iter().zip(after) {
                current.insert(path.clone(), value);
                let applied = edits
                    .keys()
                    .filter_map(|id| {
                        record
                            .applied
                            .get(id)
                            .map(|evidence| (*id, evidence.clone()))
                    })
                    .collect();
                history
                    .get_mut(&path)
                    .expect("selected path")
                    .steps
                    .push(PathStep {
                        change: *later,
                        revision: record.revision_id,
                        edits,
                        applied,
                        after: value,
                    });
            }
        }
        Ok(LaterEdits { paths: history })
    }
}
