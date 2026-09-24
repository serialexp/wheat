//! Guarded authoring from explicitly selected workspace paths.
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};

use anyhow::{Context, Result, bail};
use wheat_core::{
    ApplicationResultRecord, ContentIo, EditOperation, EditRecord, EditSubsumption, EditTransition,
    RevisionEdits, RevisionId,
    edit_capture::{FileSnapshot, capture_edit},
    guarded::{GuardedEdit, Limits, authoring},
};
use wheat_store::{
    ApplicationMapStore, ContentStore, ImmutableObjectStore, MerkleTreeStore, RepositoryHead,
    RepositoryReader, RevisionMapStore,
};

use crate::{command_output::CliFailure, workspace_files::read_workspace_file};

pub(crate) async fn author_from_workspace<O: ImmutableObjectStore>(
    root: &Path,
    objects: &O,
    store: &ContentStore,
    _head: RepositoryHead,
    effective_view: wheat_core::ViewId,
    change: wheat_core::ChangeId,
    predecessor: RevisionId,
    paths: &[String],
    guards: &BTreeMap<String, Vec<Range<usize>>>,
) -> Result<RevisionEdits> {
    if paths.is_empty() {
        return Err(CliFailure::invalid(
            "at least one explicit workspace path is required",
        ));
    }
    let reader = RepositoryReader::new(objects, store);
    let working = reader.view(effective_view).await?;
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
    // Edits are captured against the view the predecessor is currently applied to, which the
    // mutation layer also authors the new revision against. After an ancestor revision
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

    let selected = paths.iter().cloned().collect::<BTreeSet<_>>();
    if selected.len() != paths.len() {
        return Err(CliFailure::invalid("workspace paths must be unique"));
    }
    if guards.keys().any(|path| !selected.contains(path)) {
        return Err(CliFailure::invalid("a guard refers to an unselected path"));
    }

    let limits = Limits::default();
    let mut edits = old_definitions
        .iter()
        .filter(|(_, edit)| !selected.contains(&edit.path))
        .map(|(id, edit)| (*id, edit.clone()))
        .collect::<BTreeMap<_, _>>();
    let old_ids = old_definitions.keys().copied().collect::<BTreeSet<_>>();
    // Candidate lineage is emitted only from the normalizer's final fixed-point
    // components. Path equality alone is not evidence that two edits interacted.
    let mut component_lineage = Vec::new();

    for path in paths {
        let workspace = read_workspace_file(root, Path::new(path), limits.max_input_bytes)
            .with_context(|| format!("cannot read workspace path {path:?}"))?;
        let components = path.split('/').map(str::to_owned).collect::<Vec<_>>();
        let a = trees.lookup(authored.tree, &components).await?;
        let b = trees.lookup(baseline, &components).await?;
        let ranges = guards.get(path).map(Vec::as_slice);
        let old_on_path = old_definitions
            .iter()
            .filter(|(_, e)| e.path == *path)
            .map(|(id, e)| (*id, e))
            .collect::<Vec<_>>();

        let can_normalize = matches!((a, b, workspace.as_ref()), (Some(a), Some(b), Some(w))
            if a.kind == b.kind && b.kind == w.kind && b.kind != wheat_core::FileKind::Symlink)
            && old_on_path
                .iter()
                .all(|(_, e)| matches!(e.operation, EditOperation::GuardedText { .. }));
        if can_normalize {
            let a = a.unwrap();
            let b = b.unwrap();
            let w = workspace.as_ref().unwrap();
            let authored_bytes = store
                .resolve(a.content)
                .context("cannot resolve authored file")?;
            let baseline_bytes = store
                .resolve(b.content)
                .context("cannot resolve applied baseline file")?;
            let mut definitions = Vec::with_capacity(old_on_path.len());
            for (_, record) in &old_on_path {
                definitions.push(resolve_guard(record, store)?);
            }
            let normalized = authoring::normalize_with_lineage(
                &authored_bytes,
                &definitions,
                &baseline_bytes,
                &canonical_workspace(w),
                ranges,
                limits,
            )
            .map_err(|e| anyhow::anyhow!("cannot normalize {path:?}: {e:?}"))?;
            for definition in normalized.edits {
                let record = guarded_record(path, a.kind, definition, store)?;
                edits.insert(record.id(), record);
            }
            for component in normalized.components {
                let replacement = component
                    .replacement
                    .map(|definition| guarded_record(path, a.kind, definition, store))
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
                .map(|v| store.resolve(v.content))
                .transpose()
                .context("cannot resolve authored file")?;
            let captured = capture_edit(
                path,
                a.zip(before_bytes.as_deref())
                    .map(|(v, bytes)| FileSnapshot {
                        kind: v.kind,
                        bytes,
                    }),
                workspace.as_ref().map(|w| FileSnapshot {
                    kind: w.kind,
                    bytes: &w.bytes,
                }),
                ranges,
                limits,
            )
            .with_context(|| format!("cannot capture workspace path {path:?}"))?;
            persist_content(store, captured.content)?;
            edits.extend(captured.edits);
        }
    }
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
    Ok(RevisionEdits {
        edits,
        transition: EditTransition {
            preserved,
            added,
            removed,
            subsumed,
            resolutions: vec![],
        },
    })
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
        before: store.resolve(before)?,
        expected: store.resolve(expected)?,
        replacement: store.resolve(replacement)?,
        after: store.resolve(after)?,
        start_anchor,
        end_anchor,
    })
}

fn guarded_record(
    path: &str,
    kind: wheat_core::FileKind,
    d: GuardedEdit,
    store: &ContentStore,
) -> Result<EditRecord> {
    let before = store.put(&d.before)?;
    let expected = store.put(&d.expected)?;
    let replacement = store.put(&d.replacement)?;
    let after = store.put(&d.after)?;
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

pub(crate) fn persist_content(
    store: &ContentStore,
    content: BTreeMap<wheat_core::ContentId, Vec<u8>>,
) -> Result<()> {
    for (expected, bytes) in content {
        let actual = store.put(&bytes)?;
        if actual != expected {
            bail!("captured content identity mismatch");
        }
    }
    Ok(())
}
