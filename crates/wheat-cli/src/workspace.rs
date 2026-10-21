//! Guarded authoring from explicitly selected workspace paths.
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};

use anyhow::{Context, Result, bail};
use wheat_core::{
    ApplicationResultRecord, ChangeId, EditId, EditOperation, EditRecord, EditSubsumption,
    EditTransition, RevisionEdits, RevisionId, RevisionMetadata, RevisionRequest, TreeId,
    VerifiedContent, ViewId,
    edit_capture::{FileSnapshot, capture_edit},
    edit_placement::LaterEdit,
    guarded::{GuardedEdit, Limits, authoring},
};
use wheat_store::{
    ApplicationMapStore, AuthoringSession, ContentStore, ContentWriteBatch, ImmutableObjectStore,
    MerkleTreeStore, PersistedRepositoryMutator, PublicationStore, RepositoryReader,
    RevisionMapStore,
};

use crate::{
    command_output::CliFailure,
    edit_move::{FileBytes, PathPlacement, describe, place_path},
    workspace_files::{absent_where_shape_changed, read_workspace_file},
};

/// The revisions one `add` publishes: the destination change, plus every later change that
/// gives up edits moved into it.
pub(crate) struct Authored {
    pub(crate) requests: BTreeMap<ChangeId, RevisionRequest>,
    /// One line per move, empty when every edit fits the destination as it is.
    pub(crate) moves: Vec<String>,
    pub(crate) moved: Vec<MovedEdit>,
}

pub(crate) struct MovedEdit {
    pub(crate) path: String,
    pub(crate) from: ChangeId,
    pub(crate) edit: EditId,
}

/// Where `add` reads the workspace and publishes the revisions.
pub(crate) struct AddTarget<'a, 'm, O, P> {
    pub(crate) root: &'a Path,
    pub(crate) objects: &'a O,
    pub(crate) store: &'a ContentStore,
    pub(crate) mutator: &'a PersistedRepositoryMutator<'m, O, P>,
    pub(crate) session: &'a mut AuthoringSession,
    pub(crate) effective_view: ViewId,
    /// The tree the workspace was materialized from, if any.
    pub(crate) pinned: Option<TreeId>,
}

#[tracing::instrument(level = "debug", skip_all, fields(paths = paths.len()))]
pub(crate) async fn author_from_workspace<O: ImmutableObjectStore, P: PublicationStore>(
    target: &mut AddTarget<'_, '_, O, P>,
    change: ChangeId,
    predecessor: RevisionId,
    paths: &[String],
    guards: &BTreeMap<String, Vec<Range<usize>>>,
    metadata: RevisionMetadata,
) -> Result<Authored> {
    let (root, objects, store) = (target.root, target.objects, target.store);
    if paths.is_empty() {
        return Err(CliFailure::invalid(
            "at least one explicit workspace path is required",
        ));
    }
    let reader = RepositoryReader::new(objects, store);
    let working = reader.view(target.effective_view).await?;
    let current = RevisionMapStore::new(objects)
        .get(working.revisions, change)
        .await?;
    if current != Some(predecessor) {
        return Err(CliFailure::stale(
            "predecessor is not the exact current revision of the change; refresh and retry",
        ));
    }
    let revision = reader.revision(predecessor).await?;
    let application_id = ApplicationMapStore::new(objects)
        .get(working.applications, change)
        .await?
        .context("change has no current application")?;
    let application = reader.application(application_id).await?;
    if application.revision_id != predecessor {
        return Err(CliFailure::corrupt(
            "current application does not correspond to predecessor",
        ));
    }
    // Edits are captured against the change's spot, the view it is applied to, which the
    // mutation layer also authors the new revision against. After an earlier revision
    // reapplies this change, that target differs from `revision.authored_against`; the existing
    // definitions are still checked to reproduce the applied baseline on it during normalization.
    let ApplicationResultRecord::Clean { tree: baseline } = application.result else {
        return Err(CliFailure::conflict(
            "current application is conflicted; resolve it before adding",
        ));
    };
    let authored = reader.view(application.target_view).await?;
    let trees = MerkleTreeStore::new(objects);
    let mut old_definitions = BTreeMap::new();
    for id in &revision.edits {
        old_definitions.insert(*id, reader.edit(*id).await?);
    }
    // Index the predecessor's definitions by path once; scanning them per selected path would
    // make a large add against a large change quadratic.
    let mut old_by_path = BTreeMap::<&str, Vec<(EditId, &EditRecord)>>::new();
    for (id, edit) in &old_definitions {
        old_by_path
            .entry(edit.path.as_str())
            .or_default()
            .push((*id, edit));
    }

    let selected = paths.iter().cloned().collect::<BTreeSet<_>>();
    if selected.len() != paths.len() {
        return Err(CliFailure::invalid("workspace paths must be unique"));
    }
    if guards.keys().any(|path| !selected.contains(path)) {
        return Err(CliFailure::invalid("a guard refers to an unselected path"));
    }
    // Resolve every selected path in both trees with one sorted traversal each; a lookup per
    // path would decode a shared directory once for every file below it.
    let sorted = selected.iter().map(String::as_str).collect::<Vec<_>>();
    let authored_values = sorted
        .iter()
        .copied()
        .zip(trees.lookup_many(authored.tree, &sorted).await?)
        .collect::<BTreeMap<_, _>>();
    let baseline_values = sorted
        .iter()
        .copied()
        .zip(trees.lookup_many(baseline, &sorted).await?)
        .collect::<BTreeMap<_, _>>();

    let limits = Limits::default();
    let placements =
        place_later_edits(target, working.tree, change, &selected, guards, limits).await?;

    let mut edits = old_definitions
        .iter()
        .filter(|(_, edit)| !selected.contains(&edit.path))
        .map(|(id, edit)| (*id, edit.clone()))
        .collect::<BTreeMap<_, _>>();
    let old_ids = old_definitions.keys().copied().collect::<BTreeSet<_>>();
    // Candidate lineage is emitted only from the normalizer's final fixed-point
    // components. Path equality alone is not evidence that two edits interacted.
    let mut component_lineage = Vec::new();
    // Captured content is retained in bounded batches instead of one durable write per blob.
    // Every blob is committed below, before the edits referring to it are returned.
    let mut content = ContentWriteBatch::new(store);

    for path in paths {
        // Nothing in this loop awaits, so the entered span stays on this task.
        let _span = tracing::debug_span!("capture_path", path = path.as_str()).entered();
        // The change's new version of the path: the workspace file, or its placement at the
        // change's spot when later changes also edit it.
        let file = match placements.get(path) {
            Some(placement) => placement.below.clone(),
            None => absent_where_shape_changed(read_workspace_file(
                root,
                Path::new(path),
                limits.max_input_bytes,
            ))
            .with_context(|| format!("cannot read workspace path {path:?}"))?
            .map(|snapshot| FileBytes {
                kind: snapshot.kind,
                bytes: canonical_workspace(&snapshot),
            }),
        };
        let a = authored_values[path.as_str()];
        let b = baseline_values[path.as_str()];
        let ranges = guards.get(path).map(Vec::as_slice);
        let old_on_path = old_by_path
            .get(path.as_str())
            .map_or(&[][..], Vec::as_slice);

        let can_normalize = matches!((a, b, file.as_ref()), (Some(a), Some(b), Some(w))
            if a.kind == b.kind && b.kind == w.kind && b.kind != wheat_core::FileKind::Symlink)
            && old_on_path
                .iter()
                .all(|(_, e)| matches!(e.operation, EditOperation::GuardedText { .. }));
        if can_normalize {
            let a = a.unwrap();
            let b = b.unwrap();
            let w = file.as_ref().unwrap();
            let authored_bytes = store
                .get(a.content)
                .context("cannot resolve authored file")?;
            let baseline_bytes = store
                .get(b.content)
                .context("cannot resolve applied baseline file")?;
            let mut definitions = Vec::with_capacity(old_on_path.len());
            for (_, record) in old_on_path {
                definitions.push(resolve_guard(record, store)?);
            }
            let normalized = authoring::normalize_with_lineage(
                &authored_bytes,
                &definitions,
                &baseline_bytes,
                &w.bytes,
                ranges,
                limits,
            )
            .map_err(|e| anyhow::anyhow!("cannot normalize {path:?}: {e:?}"))?;
            for definition in normalized.edits {
                let record = guarded_record(path, a.kind, definition, &mut content)?;
                edits.insert(record.id(), record);
            }
            for component in normalized.components {
                let replacement = component
                    .replacement
                    .map(|definition| guarded_record(path, a.kind, definition, &mut content))
                    .transpose()?
                    .map(|record| {
                        let id = record.id();
                        edits.insert(id, record);
                        id
                    });
                component_lineage.push((
                    component
                        .members
                        .into_iter()
                        .map(|index| old_on_path[index].0)
                        .collect::<Vec<_>>(),
                    replacement,
                ));
            }
        } else {
            let before_bytes = a
                .map(|v| store.get(v.content))
                .transpose()
                .context("cannot resolve authored file")?;
            let captured = capture_edit(
                path,
                a.zip(before_bytes.as_deref())
                    .map(|(v, bytes)| FileSnapshot {
                        kind: v.kind,
                        bytes,
                    }),
                file.as_ref().map(|w| FileSnapshot {
                    kind: w.kind,
                    bytes: &w.bytes,
                }),
                ranges,
                limits,
            )
            .with_context(|| format!("cannot capture workspace path {path:?}"))?;
            stage_content(&mut content, captured.content)?;
            edits.extend(captured.edits);
        }
    }
    content.commit()?;
    let new_ids = edits.keys().copied().collect::<BTreeSet<_>>();
    let preserved = old_ids.intersection(&new_ids).copied().collect();
    let added = new_ids
        .difference(&old_ids)
        .copied()
        .collect::<BTreeSet<_>>();
    let removed = old_ids
        .difference(&new_ids)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut subsumed = Vec::new();
    for (sources, replacement) in component_lineage {
        let Some(replacement) = replacement.filter(|id| added.contains(id)) else {
            // Undo has no replacement, and a content-identical preserved definition
            // is not an added replacement under the canonical transition model.
            continue;
        };
        for source in sources.into_iter().filter(|id| removed.contains(id)) {
            subsumed.push(EditSubsumption {
                source,
                replacements: BTreeSet::from([replacement]),
            });
        }
    }
    let destination = RevisionEdits {
        edits,
        transition: EditTransition {
            preserved,
            added,
            removed,
            subsumed,
            resolutions: vec![],
        },
    };

    let mut authored = Authored {
        requests: BTreeMap::new(),
        moves: Vec::new(),
        moved: Vec::new(),
    };
    let mut giving_up = BTreeMap::<ChangeId, (RevisionId, BTreeSet<EditId>, Vec<&str>)>::new();
    for (path, placement) in &placements {
        authored.moves.extend(
            placement
                .notices
                .iter()
                .map(|notice| describe(path, change, notice)),
        );
        for later in &placement.moved {
            let revision = placement.revisions[&later.change];
            let entry = giving_up
                .entry(later.change)
                .or_insert_with(|| (revision, BTreeSet::new(), Vec::new()));
            entry.1.insert(later.edit);
            if entry.2.last() != Some(&path.as_str()) {
                entry.2.push(path);
            }
            authored.moved.push(MovedEdit {
                path: path.clone(),
                from: later.change,
                edit: later.edit,
            });
        }
    }
    for (from, (revision_id, moved, moved_paths)) in giving_up {
        let revision = reader.revision(revision_id).await?;
        let mut kept = BTreeMap::new();
        for id in revision.edits.difference(&moved) {
            kept.insert(*id, reader.edit(*id).await?);
        }
        authored.requests.insert(
            from,
            RevisionRequest {
                predecessor: revision_id,
                metadata: RevisionMetadata {
                    message: format!(
                        "Move edits on {} into change {change}",
                        moved_paths.join(", ")
                    ),
                    ..metadata.clone()
                },
                edits: RevisionEdits {
                    transition: EditTransition {
                        preserved: kept.keys().copied().collect(),
                        removed: moved,
                        ..EditTransition::default()
                    },
                    edits: kept,
                },
            },
        );
    }
    authored.requests.insert(
        change,
        RevisionRequest {
            predecessor,
            metadata,
            edits: destination,
        },
    );
    Ok(authored)
}

/// A placement with the current revision of every change it moves edits out of.
struct Placed {
    below: Option<FileBytes>,
    moved: BTreeSet<LaterEdit>,
    notices: Vec<wheat_core::edit_placement::MoveNotice>,
    revisions: BTreeMap<ChangeId, RevisionId>,
}

/// Places every selected path that a change after `change` also edits.
///
/// A change that gives up edits gets a new revision, so its remaining edits must apply exactly;
/// when a move adds such a change, every path is placed again with it.
#[tracing::instrument(level = "debug", skip_all)]
async fn place_later_edits<O: ImmutableObjectStore, P: PublicationStore>(
    target: &mut AddTarget<'_, '_, O, P>,
    effective_tree: TreeId,
    change: ChangeId,
    selected: &BTreeSet<String>,
    guards: &BTreeMap<String, Vec<Range<usize>>>,
    limits: Limits,
) -> Result<BTreeMap<String, Placed>> {
    let (root, store) = (target.root, target.store);
    let later = target
        .mutator
        .later_edits(&mut *target.session, change, selected)
        .await?;
    let edited = later
        .paths
        .into_iter()
        .filter(|(_, history)| !history.steps.is_empty())
        .collect::<BTreeMap<_, _>>();
    if edited.is_empty() {
        return Ok(BTreeMap::new());
    }
    let trees = MerkleTreeStore::new(target.objects);
    let paths = edited.keys().map(String::as_str).collect::<Vec<_>>();
    let effective = trees.lookup_many(effective_tree, &paths).await?;
    let shown = match target.pinned {
        Some(pinned) => trees.lookup_many(pinned, &paths).await?,
        None => effective.clone(),
    };
    // The workspace may predate some later changes; only the steps it shows were seen.
    let mut histories = BTreeMap::new();
    let mut current = Vec::new();
    for ((path, history), (effective, shown)) in
        edited.into_iter().zip(effective.into_iter().zip(shown))
    {
        let history = if shown == effective {
            history
        } else {
            history.up_to(shown).ok_or_else(|| {
                CliFailure::stale(format!(
                    "{path:?} changed in the working view since this workspace was materialized; run `wheat materialize` and retry"
                ))
            })?
        };
        if !history.steps.is_empty() {
            histories.insert(path, history);
            current.push(shown);
        }
    }
    if let Some(path) = histories.keys().find(|path| guards.contains_key(*path)) {
        return Err(CliFailure::invalid(format!(
            "--guard cannot be used on {path:?}: changes after {change} also edit it"
        )));
    }
    let sorted = histories.keys().map(String::as_str).collect::<Vec<_>>();
    let mut files = BTreeMap::new();
    for path in &sorted {
        let file = absent_where_shape_changed(read_workspace_file(
            root,
            Path::new(path),
            limits.max_input_bytes,
        ))
        .with_context(|| format!("cannot read workspace path {path:?}"))?
        .map(|snapshot| FileBytes {
            kind: snapshot.kind,
            bytes: canonical_workspace(&snapshot),
        });
        files.insert(*path, file);
    }
    let mut reauthored = BTreeSet::new();
    loop {
        let mut placements = BTreeMap::new();
        let mut giving_up = BTreeSet::new();
        for (path, current) in sorted.iter().zip(&current) {
            let history = &histories[*path];
            let PathPlacement {
                below,
                moved,
                notices,
            } = place_path(
                store,
                path,
                history,
                *current,
                files[path].as_ref(),
                &reauthored,
            )?;
            giving_up.extend(moved.iter().map(|edit| edit.change));
            let revisions = history
                .steps
                .iter()
                .map(|step| (step.change, step.revision))
                .collect();
            placements.insert(
                (*path).to_owned(),
                Placed {
                    below,
                    moved,
                    notices,
                    revisions,
                },
            );
        }
        if giving_up.is_subset(&reauthored) {
            return Ok(placements);
        }
        reauthored.extend(giving_up);
    }
}

fn canonical_workspace(snapshot: &crate::workspace_files::WorkspaceFileSnapshot) -> Vec<u8> {
    match wheat_core::text_capture::capture(&snapshot.bytes) {
        wheat_core::text_capture::CapturedBytes::Text(v) => v.into_owned().into_bytes(),
        wheat_core::text_capture::CapturedBytes::Binary(v) => v.into_owned(),
    }
}

fn resolve_guard(record: &EditRecord, store: &ContentStore) -> Result<GuardedEdit> {
    let EditOperation::GuardedText {
        before,
        expected,
        replacement,
        after,
        start_anchor,
        end_anchor,
        ..
    } = record.operation
    else {
        bail!("expected guarded edit")
    };
    Ok(GuardedEdit {
        before: store.get(before)?,
        expected: store.get(expected)?,
        replacement: store.get(replacement)?,
        after: store.get(after)?,
        start_anchor,
        end_anchor,
    })
}

fn guarded_record(
    path: &str,
    kind: wheat_core::FileKind,
    d: GuardedEdit,
    content: &mut ContentWriteBatch<'_>,
) -> Result<EditRecord> {
    let before = content.put(&d.before)?;
    let expected = content.put(&d.expected)?;
    let replacement = content.put(&d.replacement)?;
    let after = content.put(&d.after)?;
    Ok(EditRecord::new(
        path,
        EditOperation::GuardedText {
            kind,
            before,
            expected,
            replacement,
            after,
            start_anchor: d.start_anchor,
            end_anchor: d.end_anchor,
        },
    )?)
}

/// Durably retains captured content in bounded batches.
pub(crate) fn persist_content(
    store: &ContentStore,
    content: BTreeMap<wheat_core::ContentId, VerifiedContent>,
) -> Result<()> {
    let mut batch = ContentWriteBatch::new(store);
    stage_content(&mut batch, content)?;
    batch.commit()?;
    Ok(())
}

/// Stages captured content, which capture already hashed; it is durable only once the
/// batch is committed.
fn stage_content(
    batch: &mut ContentWriteBatch<'_>,
    content: BTreeMap<wheat_core::ContentId, VerifiedContent>,
) -> Result<()> {
    for content in content.into_values() {
        batch.put_verified(content)?;
    }
    Ok(())
}
