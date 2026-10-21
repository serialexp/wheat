//! Planning and recoverable publication for explicit checkpoint squashes.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    str::FromStr,
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use wheat_core::{ChangeId, ObjectId, RevisionId, Timestamp, ViewId};
use wheat_store::{
    BatchObjectStore, ChangeMapStore, ConflictSetStore, ImmutableObjectStore, PublicationStore,
    RepositoryHead, RepositoryReader, RepositoryRewriter, RetainedSetStore, RevisionMapStore,
    SquashRewriteRequest, finalize_repository_rewrite,
};

use crate::{
    authoring_metadata,
    command_output::{CliFailure, Success},
    local::{LocalRepository, runtime},
    remote_config::RemoteConfig,
    rewrite_transaction::{self, RewritePlan, WorkspaceRewrite},
    squash_attribution::{Coauthors, attributed_squash_metadata},
    workspace_metadata::WorkspaceMetadata,
};

const MAX_REVISIONS: usize = 250_000;

#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    Change(Option<&'a str>),
    Stack,
}

#[derive(Serialize)]
struct Report {
    scope: &'static str,
    rewritten: Vec<String>,
    revisions: BTreeMap<String, String>,
    views: BTreeMap<String, String>,
    reapplied: Vec<String>,
    reapplied_descendants: Vec<String>,
    workspaces: usize,
    retained_authority_roots: Vec<String>,
    retained_authority_view_sets: Vec<String>,
    retained_authority_revision_sets: Vec<String>,
    retained_authority_count: usize,
    consolidated_conflicts: usize,
    objects_deleted: bool,
    gc_candidates_created: bool,
    gc_candidate_revisions: Vec<String>,
    gc_candidate_views: Vec<String>,
    old_objects_status: &'static str,
    head_before: String,
    head_after: String,
    no_op: bool,
}

#[derive(Default)]
struct RetainedAuthorities {
    roots: BTreeSet<ViewId>,
    view_sets: BTreeSet<wheat_core::RetainedSetId>,
    revision_sets: BTreeSet<wheat_core::RetainedSetId>,
}

pub(crate) fn execute(
    workspace: &Path,
    explicit_store: Option<&Path>,
    scope: Scope<'_>,
    coauthors: Coauthors,
) -> Result<Success> {
    let context = crate::resolve_context(workspace, explicit_store)?;
    let local = crate::local::open(&context.store_root)?;
    // Store admission recovers any prior transaction and remains held from the first authoritative
    // read through staging, the repository CAS, and workspace finalization. No other command can
    // advance the head to an unrecoverable third value in that interval.
    let operation = rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
    let invoking = WorkspaceMetadata::read(&context.root)?;
    let expected = local.heads.read()?;
    let remote = RemoteConfig::read(&context.store_root)?;
    let actor = authoring_metadata::actor(&context.store_root)?;
    let now = authoring_metadata::now()?;
    let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
    let planned = runtime()?.block_on(plan(
        &context.store_root,
        &local,
        &objects,
        &invoking,
        expected,
        remote.as_ref(),
        scope,
        &actor,
        now,
        coauthors,
    ))?;
    let Some((request, workspaces, rewritten)) = planned else {
        return report(
            scope,
            expected,
            expected,
            Vec::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            Vec::new(),
            0,
            RetainedAuthorities::default(),
            0,
            true,
        );
    };
    let authorities = RetainedAuthorities {
        roots: request.authority_roots.clone(),
        view_sets: request.authority_view_sets.clone(),
        revision_sets: request.authority_revision_sets.clone(),
    };
    let rewriter = RepositoryRewriter::new(&objects, &local.content, &local.heads);
    let Some(prepared) = runtime()?.block_on(rewriter.prepare_squash(
        expected,
        invoking.basis,
        invoking.effective_view(),
        request,
    ))?
    else {
        return report(
            scope,
            expected,
            expected,
            Vec::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            Vec::new(),
            0,
            authorities,
            0,
            true,
        );
    };

    let mapping = prepared.mapping().clone();
    runtime()?.block_on(validate_view_mapping(&objects, &local, &mapping.views))?;
    let consolidated_conflicts = runtime()?.block_on(consolidated_conflict_count(
        &objects,
        &local,
        invoking.effective_view(),
        prepared.resulting_view(),
    ))?;
    let proposed = prepared.proposed();
    let workspace_count = workspaces.len();
    let workspaces = workspaces
        .into_iter()
        .map(|(root, old)| {
            let mut new = old.clone();
            map_workspace(&mut new, expected, proposed, &mapping.views);
            WorkspaceRewrite { root, old, new }
        })
        .collect();
    runtime()?.block_on(objects.flush())?;
    operation.stage(
        &context.store_root,
        RewritePlan {
            expected_head: expected,
            proposed_head: proposed,
            workspaces,
        },
    )?;

    ensure!(
        local.heads.read()? == expected,
        CliFailure::stale("repository head changed while squash was planned")
    );
    ensure!(
        RemoteConfig::read(&context.store_root)? == remote,
        CliFailure::stale("remote observation changed while squash was planned")
    );
    let after = finalize_repository_rewrite(&local.heads, prepared)?;
    operation.recover_staged(&context.store_root, &local)?;
    report(
        scope,
        expected,
        after,
        rewritten,
        mapping.revisions,
        mapping.views,
        mapping.reapplied,
        workspace_count,
        authorities,
        consolidated_conflicts,
        false,
    )
}

async fn plan<O: ImmutableObjectStore>(
    store_root: &Path,
    local: &LocalRepository,
    objects: &O,
    invoking: &WorkspaceMetadata,
    expected: RepositoryHead,
    remote: Option<&RemoteConfig>,
    scope: Scope<'_>,
    actor: &wheat_core::Actor,
    now: Timestamp,
    coauthors: Coauthors,
) -> Result<
    Option<(
        SquashRewriteRequest,
        Vec<(std::path::PathBuf, WorkspaceMetadata)>,
        Vec<ChangeId>,
    )>,
> {
    let reader = RepositoryReader::new(objects, &local.content);
    let envelope = reader.envelope(expected.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let effective = reader.view(invoking.effective_view()).await?;
    let revisions = collect_revisions(objects, effective.revisions).await?;
    let basis_revisions =
        collect_revisions(objects, reader.view(invoking.basis).await?.revisions).await?;
    let active_changes = revisions
        .iter()
        .filter(|(change, revision)| basis_revisions.get(change) != Some(revision))
        .map(|(change, _)| *change)
        .collect::<BTreeSet<_>>();
    let selected: BTreeSet<ChangeId> = match scope {
        Scope::Change(value) => {
            let change = value
                .map(ChangeId::from_str)
                .transpose()
                .context("invalid change ID")?
                .or(invoking.selected_change);
            let change = match change {
                Some(change) => change,
                None => {
                    return Err(missing_selection_error(
                        objects,
                        &reader,
                        state.changes,
                        &active_changes,
                    )
                    .await);
                }
            };
            ensure!(
                active_changes.contains(&change),
                CliFailure::invalid(format!("change {change} is not active in this workspace"))
            );
            [change].into_iter().collect()
        }
        Scope::Stack => revisions
            .iter()
            .filter_map(|(change, revision)| {
                (basis_revisions.get(change) != Some(revision)).then_some(*change)
            })
            .collect(),
    };
    if selected.is_empty() {
        return Ok(None);
    }

    let published_state = if let Some(config) = remote {
        let remote_head = config.last_fetched_head()?;
        let remote_envelope = reader.envelope(remote_head.envelope).await.map_err(|_| {
            CliFailure::corrupt("remote.json observed envelope is not retained locally")
        })?;
        Some(reader.state(remote_envelope.state).await?)
    } else {
        None
    };
    let published_root = published_state
        .as_ref()
        .map(|state| state.retained_revisions);
    let retained = RetainedSetStore::new(objects);
    let mut predecessors = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut rewritten = Vec::new();
    for change in selected {
        let latest = revisions[&change];
        let boundary = if let Some(root) = published_root {
            first_retained_predecessor(&reader, &retained, root, latest).await?
        } else {
            basis_revisions.get(&change).copied()
        };
        if boundary == Some(latest) {
            continue;
        }
        let records = suffix(&reader, latest, boundary).await?;
        if records.len() <= 1 {
            continue;
        }
        // `suffix` walks back from the latest checkpoint; attribution folds oldest first.
        let oldest_first = records
            .iter()
            .rev()
            .map(|(_, record)| &record.metadata)
            .collect::<Vec<_>>();
        let replacement = attributed_squash_metadata(&oldest_first, actor, now, coauthors)?;
        predecessors.insert(change, boundary);
        metadata.insert(change, replacement);
        rewritten.push(change);
    }
    if predecessors.is_empty() {
        return Ok(None);
    }

    let mut workspaces = Vec::new();
    let mut roots = BTreeSet::new();
    for (root, metadata) in rewrite_transaction::preflight_workspaces(store_root)? {
        ensure!(
            metadata.pending_repository_operation.is_none(),
            CliFailure::stale(format!(
                "workspace {} has an unresolved repository operation",
                root.display()
            ))
        );
        ensure!(
            !root.join(".wheat/materialization.json").exists(),
            CliFailure::stale(format!(
                "workspace {} has an unresolved materialization; recover it before squashing",
                root.display()
            ))
        );
        roots.insert(metadata.basis);
        roots.insert(metadata.observed_label_binding);
        roots.extend(metadata.selected_frontier);
        if let Some(materialized) = &metadata.materialized {
            roots.insert(materialized.view);
        }
        workspaces.push((root, metadata));
    }
    let labels = reader.view_label_manifest(envelope.labels).await?;
    roots.extend(labels.labels.values().map(|label| label.view));
    roots.insert(state.base_view);
    roots.insert(invoking.effective_view());

    Ok(Some((
        SquashRewriteRequest {
            expected_revisions: revisions,
            retained_predecessors: predecessors,
            revision_metadata: metadata,
            authority_roots: roots,
            authority_view_sets: published_state
                .as_ref()
                .map(|state| state.retained_views)
                .into_iter()
                .collect(),
            authority_revision_sets: published_state
                .as_ref()
                .map(|state| state.retained_revisions)
                .into_iter()
                .collect(),
        },
        workspaces,
        rewritten,
    )))
}

async fn missing_selection_error<O: ImmutableObjectStore>(
    objects: &O,
    reader: &RepositoryReader<'_, O>,
    changes_root: wheat_core::ChangeMapId,
    active_changes: &BTreeSet<ChangeId>,
) -> anyhow::Error {
    let changes = ChangeMapStore::new(objects);
    let mut choices = String::new();
    for change in active_changes {
        let title = match changes.get(changes_root, *change).await {
            Ok(Some(object)) => match reader.change(object).await {
                Ok(record) if record.id == *change => record.metadata.title,
                _ => "<unavailable>".to_owned(),
            },
            _ => "<unavailable>".to_owned(),
        };
        choices.push_str(&format!("\n  {change}  {title}"));
    }
    if choices.is_empty() {
        choices.push_str("\n  (none)");
    }
    CliFailure::invalid(format!(
        "workspace has no selected change; pass CHANGE or run `wheat change select`\nactive changes:{choices}"
    ))
}

async fn collect_revisions<O: ImmutableObjectStore>(
    objects: &O,
    root: wheat_core::RevisionMapId,
) -> Result<BTreeMap<ChangeId, RevisionId>> {
    let mut out = BTreeMap::new();
    let mut cursor = RevisionMapStore::new(objects).cursor(root).await?;
    while let Some((change, revision)) = cursor.next().await? {
        ensure!(
            out.len() < MAX_REVISIONS,
            CliFailure::limit("squash revision count exceeds the limit")
        );
        out.insert(change, revision);
    }
    Ok(out)
}

async fn first_retained_predecessor<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    retained: &RetainedSetStore<'_, O>,
    root: wheat_core::RetainedSetId,
    mut revision: RevisionId,
) -> Result<Option<RevisionId>> {
    loop {
        if retained.contains(root, ObjectId::from(revision)).await? {
            return Ok(Some(revision));
        }
        match reader.revision(revision).await?.predecessor {
            Some(previous) => revision = previous,
            None => return Ok(None),
        }
    }
}

async fn suffix<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    mut at: RevisionId,
    boundary: Option<RevisionId>,
) -> Result<Vec<(RevisionId, wheat_core::RevisionRecord)>> {
    let mut out = Vec::new();
    while Some(at) != boundary {
        ensure!(
            out.len() < MAX_REVISIONS,
            CliFailure::limit("squash checkpoint count exceeds the limit")
        );
        let record = reader.revision(at).await?;
        let previous = record.predecessor;
        out.push((at, record));
        match previous {
            Some(previous) => at = previous,
            None if boundary.is_none() => break,
            None => {
                return Err(CliFailure::corrupt(
                    "squash boundary is not an ancestor of the selected revision",
                ));
            }
        }
    }
    Ok(out)
}

async fn validate_view_mapping<O: ImmutableObjectStore>(
    objects: &O,
    local: &LocalRepository,
    mapping: &BTreeMap<ViewId, ViewId>,
) -> Result<()> {
    let reader = RepositoryReader::new(objects, &local.content);
    for (old, new) in mapping {
        ensure!(
            reader.view(*old).await?.tree == reader.view(*new).await?.tree,
            CliFailure::corrupt("rewrite mapped views with different trees")
        );
    }
    Ok(())
}

async fn conflict_count<O: ImmutableObjectStore>(
    objects: &O,
    local: &LocalRepository,
    root: ViewId,
) -> Result<usize> {
    let reader = RepositoryReader::new(objects, &local.content);
    let view = reader.view(root).await?;
    let mut cursor = ConflictSetStore::new(objects)
        .cursor(view.unresolved_conflicts)
        .await?;
    let mut count = 0usize;
    while cursor.next().await?.is_some() {
        ensure!(
            count < crate::conflict_inspection::MAX_CONFLICTS,
            CliFailure::limit("conflict count exceeds inspection limit")
        );
        count += 1;
    }
    Ok(count)
}

async fn consolidated_conflict_count<O: ImmutableObjectStore>(
    objects: &O,
    local: &LocalRepository,
    before: ViewId,
    after: ViewId,
) -> Result<usize> {
    let reader = RepositoryReader::new(objects, &local.content);
    let old = reader.view(before).await?;
    let new = reader.view(after).await?;
    if old.unresolved_conflicts == new.unresolved_conflicts {
        return Ok(0);
    }
    let before = conflict_count(objects, local, before).await?;
    let after = conflict_count(objects, local, after).await?;
    Ok(before.saturating_sub(after))
}

fn map_view(value: &mut ViewId, mapping: &BTreeMap<ViewId, ViewId>) {
    if let Some(mapped) = mapping.get(value) {
        *value = *mapped;
    }
}
fn map_workspace(
    metadata: &mut WorkspaceMetadata,
    expected: RepositoryHead,
    proposed: RepositoryHead,
    mapping: &BTreeMap<ViewId, ViewId>,
) {
    map_view(&mut metadata.basis, mapping);
    map_view(&mut metadata.observed_label_binding, mapping);
    if let Some(view) = &mut metadata.selected_frontier {
        map_view(view, mapping);
    }
    if let Some(materialized) = &mut metadata.materialized {
        map_view(&mut materialized.view, mapping);
        if RepositoryHead::from(materialized.repository_head) == expected {
            materialized.repository_head = proposed.into();
        }
    }
    if let Some(pending) = &mut metadata.pending_repository_operation {
        map_view(&mut pending.resulting_frontier, mapping);
        if RepositoryHead::from(pending.expected_head) == expected {
            pending.expected_head = proposed.into();
        }
        if RepositoryHead::from(pending.proposed_head) == expected {
            pending.proposed_head = proposed.into();
        }
    }
}

fn report(
    scope: Scope<'_>,
    before: RepositoryHead,
    after: RepositoryHead,
    rewritten: Vec<ChangeId>,
    revisions: BTreeMap<RevisionId, RevisionId>,
    views: BTreeMap<ViewId, ViewId>,
    reapplied: Vec<ChangeId>,
    workspaces: usize,
    authorities: RetainedAuthorities,
    consolidated_conflicts: usize,
    no_op: bool,
) -> Result<Success> {
    let command = match scope {
        Scope::Change(_) => "change.squash",
        Scope::Stack => "stack.squash",
    };
    let retained_authority_count =
        authorities.roots.len() + authorities.view_sets.len() + authorities.revision_sets.len();
    let gc_candidate_revisions = revisions
        .iter()
        .filter(|(old, new)| old != new)
        .map(|(old, _)| old.to_string())
        .collect();
    let gc_candidate_views = views
        .iter()
        .filter(|(old, new)| old != new)
        .map(|(old, _)| old.to_string())
        .collect();
    let report = Report {
        scope: match scope {
            Scope::Change(_) => "change",
            Scope::Stack => "stack",
        },
        rewritten: rewritten.iter().map(ToString::to_string).collect(),
        revisions: revisions
            .into_iter()
            .map(|(old, new)| (old.to_string(), new.to_string()))
            .collect(),
        views: views
            .into_iter()
            .map(|(old, new)| (old.to_string(), new.to_string()))
            .collect(),
        reapplied: reapplied.iter().map(ToString::to_string).collect(),
        reapplied_descendants: reapplied.iter().map(ToString::to_string).collect(),
        workspaces,
        retained_authority_roots: authorities.roots.iter().map(ToString::to_string).collect(),
        retained_authority_view_sets: authorities
            .view_sets
            .iter()
            .map(ToString::to_string)
            .collect(),
        retained_authority_revision_sets: authorities
            .revision_sets
            .iter()
            .map(ToString::to_string)
            .collect(),
        retained_authority_count,
        consolidated_conflicts,
        objects_deleted: false,
        gc_candidates_created: !no_op,
        gc_candidate_revisions,
        gc_candidate_views,
        old_objects_status: if no_op { "unchanged" } else { "gc_candidates" },
        head_before: format!("{}:{}", before.generation, before.envelope),
        head_after: format!("{}:{}", after.generation, after.envelope),
        no_op,
    };
    let human = if no_op {
        format!(
            "nothing to squash; repository head and workspace metadata unchanged\nretained authorities: {}; consolidated conflicts: 0\nobjects deleted: no; old objects unchanged (no GC candidates created)\n",
            report.retained_authority_count
        )
    } else {
        let revision_details = report
            .revisions
            .iter()
            .map(|(old, new)| format!("  revision {old} -> {new}\n"))
            .collect::<String>();
        let view_details = report
            .views
            .iter()
            .map(|(old, new)| format!("  view {old} -> {new}\n"))
            .collect::<String>();
        let reapplied_details = report
            .reapplied
            .iter()
            .map(|change| format!("  reapplied descendant {change}\n"))
            .collect::<String>();
        format!(
            "squashed {} change(s); repository generation {} -> {}\n{revision_details}{view_details}{reapplied_details}retained authorities: {}; consolidated conflicts: {}\nobjects deleted: no; replaced revisions/views are GC candidates unless retained by an authority\n",
            report.rewritten.len(),
            before.generation,
            after.generation,
            report.retained_authority_count,
            report.consolidated_conflicts
        )
    };
    Success::new(command, &report, human)
}
