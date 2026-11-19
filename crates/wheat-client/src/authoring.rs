//! Writing the workspace's changes: capturing files into a change, creating and
//! selecting changes, and restoring tracked files.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};

use anyhow::{Context, Result};
use wheat_core::{ChangeId, ChangeProposal, EditId, RevisionEdits, RevisionId};
use wheat_store::{
    BatchObjectStore, ContentStore, PersistedRepositoryMutator, PublicationStore, TreePathFilter,
    finalize_mutation_publication, prepare_mutation_publication,
};

use crate::{
    active_changes,
    authoring_metadata::{self, UserConfig},
    capture_settlement,
    discovery::resolve_context,
    dot_git::DotGitGate,
    local::{open, runtime},
    materialization,
    outcome::CliFailure,
    rewrite_transaction::StoreOperationGuard,
    selection_recovery, workspace,
    workspace_metadata::{PendingRepositoryOperation, WorkspaceMetadata},
    workspace_registry_service, workspace_scan, workspace_walk,
};

/// Which workspace paths an add captures.
pub enum AddPaths {
    /// Every tracked path whose file differs from the materialized tree.
    Modified,
    Listed(Vec<String>),
}

pub struct AddRequest {
    pub paths: AddPaths,
    /// The change to capture into; the workspace's selected change when absent.
    pub change: Option<ChangeId>,
    /// The revision the new one revises; the change's current revision when absent.
    pub predecessor: Option<RevisionId>,
    /// Byte ranges per path that must keep their place in the edits.
    pub guards: BTreeMap<String, Vec<Range<usize>>>,
    pub message: String,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
}

pub struct AddOutcome {
    pub change: ChangeId,
    pub title: String,
    pub revision: RevisionId,
    pub paths: usize,
    /// One line per edit moved out of a later change, as the confirmation listed it.
    pub moves: Vec<String>,
    pub moved: Vec<MovedEdit>,
    pub reapplied: Vec<ChangeId>,
    /// The workspace's selected change afterwards.
    pub selected: Option<ChangeId>,
    /// The workspace no longer shows the working view, so `wheat materialize` is due.
    pub materialize_required: bool,
    /// Paths whose workspace file changed while the add ran.
    pub diverged: Vec<String>,
}

/// An edit a later change gave up to the destination, and that change's new revision.
pub struct MovedEdit {
    pub path: String,
    pub from: ChangeId,
    pub revision: RevisionId,
    pub edit: EditId,
}

/// Captures workspace files into a change and publishes the new revision. When that
/// moves edits out of later changes, `confirm_moves` sees them first and can refuse.
pub fn add(
    workspace: &Path,
    explicit_store: Option<&Path>,
    user: &UserConfig,
    request: AddRequest,
    confirm_moves: impl FnOnce(ChangeId, &[String]) -> Result<()>,
) -> Result<AddOutcome> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    // Store admission precedes the workspace lock. Holding both across planning and
    // publication prevents either a staged rewrite or another head mutation from
    // interleaving before the compare-and-swap.
    let _operation = StoreOperationGuard::acquire(&context.store_root, &local)?;
    let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    let runtime = runtime()?;
    runtime.block_on(materialization::recover(
        &context.root,
        &local,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &local, &mut context.workspace)?;

    let paths = match request.paths {
        AddPaths::Modified => {
            let scan = workspace_scan::scan_locked(
                &context.root,
                &local,
                &context.workspace,
                0,
                &TreePathFilter::default(),
                workspace_scan::ContentIds::Optional,
            )?;
            let resolved: Vec<String> = scan.changes.into_iter().map(|c| c.path).collect();
            if resolved.is_empty() {
                return Err(CliFailure::invalid(
                    "no modified or deleted tracked paths; workspace is clean",
                ));
            }
            resolved
        }
        AddPaths::Listed(arguments) => {
            expand_listed(&context.root, &local, &context.workspace, &arguments)?
        }
    };

    let expected = local.heads.read()?;
    let change = request
        .change
        .or(context.workspace.selected_change)
        .context("workspace has no selected change; run `wheat change new` or pass --change")?;
    let destination = runtime
        .block_on(active_changes::in_view(
            &local,
            expected,
            context.workspace.effective_view(),
            change,
        ))?
        .with_context(|| format!("change {change} is not in this workspace's view"))?;
    let predecessor = request.predecessor.unwrap_or(destination.revision);
    let title = destination.title;
    let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
    let (_, revision_metadata) = authoring_metadata::metadata(
        user,
        &context.store_root,
        authoring_metadata::MetadataArgs {
            message: request.message,
            description: None,
            created_at: None,
            authored_at: request.authored_at,
            committed_at: request.committed_at,
        },
        false,
    )?;
    let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads);
    let mut session = runtime.block_on(mutator.open_authoring(
        expected,
        context.workspace.basis,
        context.workspace.effective_view(),
    ))?;
    let authored = runtime.block_on(workspace::author_from_workspace(
        &mut workspace::AddTarget {
            root: &context.root,
            objects: &objects,
            store: &local.content,
            mutator: &mutator,
            session: &mut session,
            effective_view: context.workspace.effective_view(),
            pinned: context
                .workspace
                .materialized
                .as_ref()
                .map(|pinned| pinned.tree),
        },
        change,
        predecessor,
        &paths,
        &request.guards,
        revision_metadata,
    ))?;
    if authored.changed.is_empty() {
        return Err(CliFailure::invalid(format!(
            "nothing to add: change {change} already has what the workspace holds at {}",
            listed(&paths)
        )));
    }
    let changed = authored.changed.len();
    if !authored.moves.is_empty() {
        confirm_moves(change, &authored.moves)?;
    }
    let (prepared, outcome) =
        runtime.block_on(mutator.revise_changes_in(session, authored.requests))?;
    let revision = outcome.revisions[&change];
    let resulting_frontier = prepared.resulting_view();
    let publication = runtime.block_on(prepare_mutation_publication(
        &objects,
        &local.content,
        &local.heads,
        expected,
        prepared,
    ))?;
    // Settle the materialized pin before anything is published, so a settlement
    // failure leaves the repository and workspace metadata unchanged. Its tree
    // nodes join the batch flushed below.
    let settlement = context
        .workspace
        .materialized
        .as_ref()
        .map(|pinned| {
            runtime.block_on(capture_settlement::settle(
                &context.root,
                &objects,
                &local.content,
                pinned,
                publication.proposed(),
                resulting_frontier,
                change,
                &paths,
            ))
        })
        .transpose()?;
    context.workspace.pending_repository_operation = Some(PendingRepositoryOperation {
        expected_head: publication.expected().into(),
        proposed_head: publication.proposed().into(),
        resulting_frontier,
        selected_change: context.workspace.selected_change,
        rebinds: false,
    });
    match &settlement {
        // Root the settled pin before metadata can reference it. If the
        // publication is interrupted, recovery keeps the previous pin and the
        // next reconciliation releases this root.
        Some(settlement) => workspace_registry_service::transition_materialization(
            &context.root,
            &context.store_root,
            &context.workspace,
            settlement.materialized.repository_head.into(),
            settlement.materialized.view,
            settlement.materialized.tree,
        )?,
        None => workspace_registry_service::reconcile(
            &context.root,
            &context.store_root,
            &context.workspace,
        )?,
    }
    context
        .workspace
        .replace_prepared(&context.root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    runtime.block_on(objects.flush())?;
    finalize_mutation_publication(&local.heads, publication)?;
    context.workspace.selected_frontier =
        (resulting_frontier != context.workspace.basis).then_some(resulting_frontier);
    context.workspace.pending_repository_operation = None;
    if let Some(settlement) = &settlement {
        context.workspace.materialized = Some(settlement.materialized.clone());
    }
    context
        .workspace
        .replace_prepared(&context.root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    workspace_registry_service::reconcile(&context.root, &context.store_root, &context.workspace)?;
    let (materialize_required, diverged) = match settlement {
        Some(settlement) => (!settlement.complete, settlement.diverged),
        None => (true, Vec::new()),
    };
    Ok(AddOutcome {
        change,
        title,
        revision,
        paths: changed,
        moves: authored.moves,
        moved: authored
            .moved
            .into_iter()
            .map(|moved| MovedEdit {
                revision: outcome.revisions[&moved.from],
                path: moved.path,
                from: moved.from,
                edit: moved.edit,
            })
            .collect(),
        reapplied: outcome.reapplied,
        selected: context.workspace.selected_change,
        materialize_required,
        diverged,
    })
}

/// The paths `add` captures for `arguments`, which it takes as `git add` does: a file as
/// it is, whether the workspace still has it or not, and a directory (`.` is the
/// workspace's root) as each tracked file below it the workspace changed or deleted, and
/// each untracked file below it no ignore rule covers. Nothing inside `.git` is picked
/// up by a directory. An argument that names nothing is refused.
fn expand_listed(
    root: &Path,
    local: &crate::local::LocalRepository,
    workspace: &WorkspaceMetadata,
    arguments: &[String],
) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    // Directories the workspace has, and paths it doesn't: a deleted file or directory
    // of the tree, or nothing at all.
    let mut directories = Vec::new();
    let mut missing = Vec::new();
    for argument in arguments {
        let invalid = || CliFailure::invalid(format!("invalid workspace path {argument:?}"));
        if argument.is_empty() || argument.starts_with('/') {
            return Err(invalid());
        }
        let path = argument.trim_end_matches('/');
        if path == "." {
            directories.push(String::new());
            continue;
        }
        materialization::validate_path(path).map_err(|_| invalid())?;
        match std::fs::symlink_metadata(root.join(path)) {
            Ok(metadata) if metadata.is_dir() => directories.push(path.to_owned()),
            Ok(_) => {
                paths.insert(path.to_owned());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                missing.push(path.to_owned());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect workspace path {path:?}"));
            }
        }
    }
    if directories.is_empty() && missing.is_empty() {
        return Ok(paths.into_iter().collect());
    }

    let filter = if directories.iter().any(String::is_empty) {
        TreePathFilter::default()
    } else {
        TreePathFilter::new(directories.iter().chain(&missing).cloned())
    };
    let scan = workspace_scan::scan_locked(
        root,
        local,
        workspace,
        0,
        &filter,
        workspace_scan::ContentIds::Optional,
    )?;
    paths.extend(scan.changes.into_iter().map(|change| change.path));
    for path in &missing {
        let below = format!("{path}/");
        let matched = paths.contains(path)
            || paths
                .range(below.clone()..)
                .next()
                .is_some_and(|found| found.starts_with(&below));
        if !matched {
            return Err(CliFailure::invalid(format!(
                "{path} matches no file in the workspace or the tree"
            )));
        }
    }

    let global = wheat_ignore::loader::read_global_ignore();
    let global = global
        .as_ref()
        .map(|(content, path)| (content.as_str(), path.clone()));
    let mut ignore = wheat_ignore::stack::IgnoreStack::new(global, &[]);
    for directory in &directories {
        let Some(walk) = workspace_walk::walk_below(
            root,
            &scan.tracked_paths,
            &mut ignore,
            workspace_walk::WalkLimits::default(),
            directory,
        )?
        else {
            continue;
        };
        if walk.truncated {
            return Err(CliFailure::limit(format!(
                "{} holds more than one add walks; name its files or directories below it",
                if directory.is_empty() {
                    "the workspace"
                } else {
                    directory
                }
            )));
        }
        paths.extend(
            walk.untracked
                .into_iter()
                .filter(|entry| {
                    !entry.is_directory
                        && entry.classification
                            == workspace_walk::UntrackedClassification::Untracked
                })
                .map(|entry| entry.path),
        );
    }
    if paths.is_empty() {
        return Err(CliFailure::invalid(format!(
            "nothing to add: no changed, deleted or untracked file under {}",
            listed(&directories)
        )));
    }
    Ok(paths.into_iter().collect())
}

/// `paths` for a message: the first few, then how many more.
fn listed(paths: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = paths
        .iter()
        .take(SHOWN)
        .map(|path| if path.is_empty() { "." } else { path.as_str() })
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOWN {
        text.push_str(&format!(" and {} more", paths.len() - SHOWN));
    }
    text
}

pub struct NewChange {
    pub dependencies: BTreeSet<ChangeId>,
    /// With no dependencies, start the change on the basis rather than on the working
    /// view's frontier.
    pub independent: bool,
    pub metadata: authoring_metadata::MetadataArgs,
}

pub struct NewChangeOutcome {
    pub change: ChangeId,
    pub revision: RevisionId,
    pub reapplied: Vec<ChangeId>,
}

/// Creates a change with the edits `edits` captures into the store, publishes it and
/// selects it in the workspace.
pub fn create_change(
    workspace: &Path,
    explicit_store: Option<&Path>,
    user: &UserConfig,
    request: NewChange,
    edits: impl FnOnce(&ContentStore) -> Result<RevisionEdits>,
) -> Result<NewChangeOutcome> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation = StoreOperationGuard::acquire(&context.store_root, &local)?;
    let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    let runtime = runtime()?;
    runtime.block_on(materialization::recover(
        &context.root,
        &local,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
    let expected = local.heads.read()?;
    let use_frontier = request.dependencies.is_empty() && !request.independent;
    let (change_metadata, revision_metadata) =
        authoring_metadata::metadata(user, &context.store_root, request.metadata, true)?;
    let edits = edits(&local.content)?;
    let change = ChangeId::new();
    let proposal = ChangeProposal {
        id: change,
        metadata: change_metadata.expect("new change metadata requested"),
        dependencies: request.dependencies,
        revision_metadata,
        edits,
    };
    let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
    let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads);
    let basis = context.workspace.basis;
    let effective = context.workspace.effective_view();
    let (prepared, outcome) = if use_frontier {
        runtime.block_on(mutator.create_on_frontier(expected, basis, effective, proposal))
    } else {
        runtime.block_on(mutator.create(expected, basis, effective, proposal))
    }?;
    let resulting_frontier = prepared.resulting_view();
    let publication = runtime.block_on(prepare_mutation_publication(
        &objects,
        &local.content,
        &local.heads,
        expected,
        prepared,
    ))?;
    context.workspace.pending_repository_operation = Some(PendingRepositoryOperation {
        expected_head: publication.expected().into(),
        proposed_head: publication.proposed().into(),
        resulting_frontier,
        selected_change: Some(change),
        rebinds: false,
    });
    workspace_registry_service::reconcile(&context.root, &context.store_root, &context.workspace)?;
    context
        .workspace
        .replace_prepared(&context.root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    runtime.block_on(objects.flush())?;
    finalize_mutation_publication(&local.heads, publication)?;
    context.workspace.selected_frontier =
        (resulting_frontier != context.workspace.basis).then_some(resulting_frontier);
    context.workspace.selected_change = Some(change);
    context.workspace.pending_repository_operation = None;
    context
        .workspace
        .replace_prepared(&context.root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    workspace_registry_service::reconcile(&context.root, &context.store_root, &context.workspace)?;
    Ok(NewChangeOutcome {
        change,
        revision: outcome.revision_id,
        reapplied: outcome.reapplied,
    })
}

/// Makes `change` the one later adds capture into, and returns its title.
pub fn select_change(
    workspace: &Path,
    explicit_store: Option<&Path>,
    change: ChangeId,
) -> Result<String> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation = StoreOperationGuard::acquire(&context.store_root, &local)?;
    let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    let runtime = runtime()?;
    runtime.block_on(materialization::recover_locked(
        &context.root,
        &local,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
    let head = local.heads.read()?;
    let title = runtime
        .block_on(active_changes::in_view(
            &local,
            head,
            context.workspace.effective_view(),
            change,
        ))?
        .with_context(|| format!("change {change} is not in this workspace's view"))?
        .title;
    context.workspace.selected_change = Some(change);
    context
        .workspace
        .replace_prepared(&context.root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    workspace_registry_service::reconcile(&context.root, &context.store_root, &context.workspace)?;
    Ok(title)
}

/// Rewrites `paths` in the workspace from the materialized tree, discarding their
/// uncaptured edits. Returns how many paths it restored.
pub fn restore(
    workspace: &Path,
    explicit_store: Option<&Path>,
    paths: &[String],
    dot_git: DotGitGate,
) -> Result<usize> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let _operation = StoreOperationGuard::acquire(&context.store_root, &repository)?;
    runtime()?.block_on(materialization::restore(
        &context.root,
        &repository,
        &mut context.workspace,
        paths,
        dot_git,
    ))
}
