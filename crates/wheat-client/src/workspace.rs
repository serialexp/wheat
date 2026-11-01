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
    edit_move::{FileBytes, PathPlacement, describe, place_path},
    outcome::CliFailure,
    workspace_files::{
        WorkspaceFileError, absent_where_shape_changed, analyze_workspace_file,
        read_workspace_file, stream_workspace_file,
    },
    workspace_scan::MAX_WORKSPACE_FILE_BYTES,
};

/// The revisions one `add` publishes: the destination change, plus every later change that
/// gives up edits moved into it.
pub struct Authored {
    pub requests: BTreeMap<ChangeId, RevisionRequest>,
    /// One line per move, empty when every edit fits the destination as it is.
    pub moves: Vec<String>,
    pub moved: Vec<MovedEdit>,
}

pub struct MovedEdit {
    pub path: String,
    pub from: ChangeId,
    pub edit: EditId,
}

/// Where `add` reads the workspace and publishes the revisions.
pub struct AddTarget<'a, 'm, O, P> {
    pub root: &'a Path,
    pub objects: &'a O,
    pub store: &'a ContentStore,
    pub mutator: &'a PersistedRepositoryMutator<'m, O, P>,
    pub session: &'a mut AuthoringSession,
    pub effective_view: ViewId,
    /// The tree the workspace was materialized from, if any.
    pub pinned: Option<TreeId>,
}

#[tracing::instrument(level = "debug", skip_all, fields(paths = paths.len()))]
pub async fn author_from_workspace<O: ImmutableObjectStore, P: PublicationStore>(
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
            Some(placement) => placement.below.clone().map(Captured::Bytes),
            None => read_captured(root, path, store, limits)?,
        };
        let a = authored_values[path.as_str()];
        let b = baseline_values[path.as_str()];
        let ranges = guards.get(path).map(Vec::as_slice);
        let old_on_path = old_by_path
            .get(path.as_str())
            .map_or(&[][..], Vec::as_slice);
        // A file past the guarded bound on either side changes as a whole.
        let past_bound = |value: Option<wheat_core::FileValue>| -> Result<bool> {
            Ok(match value {
                Some(value) => store.logical_length(value.content)? > limits.max_input_bytes as u64,
                None => false,
            })
        };
        let whole = matches!(file, Some(Captured::Stored(_))) || past_bound(a)?;

        let can_normalize = !whole
            && matches!((a, b, file.as_ref()), (Some(a), Some(b), Some(Captured::Bytes(w)))
            if a.kind == b.kind && b.kind == w.kind && b.kind != wheat_core::FileKind::Symlink)
            && old_on_path
                .iter()
                .all(|(_, e)| matches!(e.operation, EditOperation::GuardedText { .. }))
            && !past_bound(b)?;
        if can_normalize {
            let a = a.unwrap();
            let b = b.unwrap();
            let Some(Captured::Bytes(w)) = file.as_ref() else {
                unreachable!("matched above")
            };
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
        } else if whole {
            if ranges.is_some_and(|ranges| !ranges.is_empty()) {
                return Err(CliFailure::invalid(format!(
                    "--guard cannot be used on {path:?}: it is over {} bytes, so it changes as a whole",
                    limits.max_input_bytes
                )));
            }
            let after = match file {
                None => None,
                Some(Captured::Stored(value)) => Some(value),
                Some(Captured::Bytes(w)) => Some(wheat_core::FileValue {
                    content: content.put(&w.bytes)?,
                    kind: w.kind,
                }),
            };
            if a != after {
                let record = EditRecord::new(
                    path,
                    EditOperation::ExactFile {
                        expected: a,
                        replacement: after,
                    },
                )?;
                edits.insert(record.id(), record);
            }
        } else {
            let file = match &file {
                Some(Captured::Bytes(w)) => Some(w),
                Some(Captured::Stored(_)) => unreachable!("a stored file changes as a whole"),
                None => None,
            };
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
    // Placing an edit reads every version of the file whole, so a file past the guarded
    // bound cannot be placed yet.
    let past_bound = |path: &str| {
        CliFailure::invalid(format!(
            "cannot add {path:?} to change {change}: changes after it also edit it, and placing an edit to a file over {} bytes among later changes is not supported yet",
            limits.max_input_bytes
        ))
    };
    for (path, current) in sorted.iter().zip(&current) {
        for value in [*current, histories[*path].below].into_iter().flatten() {
            if store.logical_length(value.content)? > limits.max_input_bytes as u64 {
                return Err(past_bound(path));
            }
        }
    }
    let mut files = BTreeMap::new();
    for path in &sorted {
        let read = absent_where_shape_changed(read_workspace_file(
            root,
            Path::new(path),
            limits.max_input_bytes,
        ));
        if matches!(read, Err(WorkspaceFileError::TooLarge { .. })) {
            return Err(past_bound(path));
        }
        let file = read
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

/// A path's new version as capture reads it.
enum Captured {
    /// Its canonical bytes.
    Bytes(FileBytes),
    /// A file past the guarded bound, already streamed into the store.
    Stored(wheat_core::FileValue),
}

/// Reads the workspace version of `path`. A file past the guarded bound is streamed
/// into the store rather than read whole.
fn read_captured(
    root: &Path,
    path: &str,
    store: &ContentStore,
    limits: Limits,
) -> Result<Option<Captured>> {
    let read = absent_where_shape_changed(read_workspace_file(
        root,
        Path::new(path),
        limits.max_input_bytes,
    ));
    let file = match read {
        Err(WorkspaceFileError::TooLarge { .. }) => {
            return Ok(store_long_file(root, path, store)?.map(Captured::Stored));
        }
        read => read.with_context(|| format!("cannot read workspace path {path:?}"))?,
    };
    Ok(file.map(|snapshot| {
        Captured::Bytes(FileBytes {
            kind: snapshot.kind,
            bytes: canonical_workspace(&snapshot),
        })
    }))
}

/// Streams a workspace file too long to hold into the store and returns its value. One
/// pass finds its canonical ID, as status does; unless the store already has that, a
/// second writes it, and the store checks the bytes are the same.
#[tracing::instrument(level = "debug", skip_all, fields(path = path))]
fn store_long_file(
    root: &Path,
    path: &str,
    store: &ContentStore,
) -> Result<Option<wheat_core::FileValue>> {
    let cannot = || format!("cannot read workspace path {path:?}");
    let Some(file) = absent_where_shape_changed(analyze_workspace_file(
        root,
        Path::new(path),
        MAX_WORKSPACE_FILE_BYTES,
        0,
    ))
    .with_context(cannot)?
    else {
        return Ok(None);
    };
    let value = wheat_core::FileValue {
        content: file.content,
        kind: file.kind,
    };
    if store.contains(value.content) {
        return Ok(Some(value));
    }
    let changed = || {
        CliFailure::stale(format!(
            "{path:?} changed while it was being added; retry once it is written"
        ))
    };
    let mut writer = store.writer();
    let kind = stream_workspace_file(
        root,
        Path::new(path),
        file.text,
        MAX_WORKSPACE_FILE_BYTES,
        &mut |bytes| std::io::Write::write_all(&mut writer, bytes),
    )
    .with_context(cannot)?;
    if kind != Some(file.kind) {
        return Err(changed());
    }
    match writer.finish(Some(value.content)) {
        Ok(_) => Ok(Some(value)),
        Err(wheat_store::ContentStoreError::Corrupt { .. }) => Err(changed()),
        Err(error) => Err(error).with_context(|| format!("cannot store {path:?}")),
    }
}

/// Durably retains captured content in bounded batches.
pub fn persist_content(
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
