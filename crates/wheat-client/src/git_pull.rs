//! `wheat pull <git-url>`: append a Git branch's new commits to the active label's
//! history and move the workspace's local work above them, as `git pull --rebase`
//! would.
//!
//! The label stays where it is: the result is the label's single frontier, which the
//! workspace switches to. Local work is rebased like Git rebases it: each moved
//! revision keeps its author and message, and the puller commits it now. A moved change
//! that no longer applies is recorded as a durable conflict.

use std::{fmt::Write as _, path::Path};

use anyhow::{Context, Result};
use serde_json::json;
use wheat_core::{ChangeId, RevisionMetadata};
use wheat_git::pull::{PullLimits, prepare_pull};
use wheat_store::{
    BatchObjectStore, PersistedRepositoryMutator, PublicationStore, RepositoryReader,
    finalize_mutation_publication, prepare_mutation_publication,
};

use crate::{
    authoring_metadata,
    discovery::resolve_context,
    dot_git::DotGitGate,
    frontier_service,
    local::{open, projection_path, runtime},
    materialization,
    outcome::{CliFailure, Success},
    rewrite_transaction, selection_recovery,
    workspace_metadata::WorkspaceMetadata,
    workspace_scan::ensure_workspace_clean_locked,
};

#[tracing::instrument(name = "git_pull", level = "debug", skip_all)]
pub fn pull(
    workspace: &Path,
    explicit_store: Option<&Path>,
    url: &str,
    from: Option<&str>,
    dot_git: DotGitGate,
) -> Result<Success> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &repository)?;
    let _lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    let runtime = runtime()?;
    runtime.block_on(materialization::recover_locked(
        &context.root,
        &repository,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &repository, &mut context.workspace)?;
    ensure_workspace_clean_locked(&context.root, &repository, &context.workspace)?;

    let label = context.workspace.active_label.clone();
    let listed = runtime.block_on(frontier_service::list(&repository, &label))?;
    // The head `list` read, unless a command published since; the publication below
    // refuses that.
    let expected = repository.heads.read()?;
    let effective = match listed.frontiers.as_slice() {
        [] => listed.basis,
        [frontier] => *frontier,
        frontiers => {
            return Err(CliFailure::invalid(format!(
                "label {label} has {} frontiers; join them with `wheat integrate` before \
                 pulling",
                frontiers.len()
            )));
        }
    };
    let (original_git_ref, projected_commits) = runtime.block_on(async {
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.envelope(expected.envelope).await?;
        let labels = reader.view_label_manifest(envelope.labels).await?;
        let state = reader.state(envelope.state).await?;
        Ok::<_, anyhow::Error>((
            labels
                .labels
                .get(&label)
                .and_then(|label| label.original_git_ref.clone()),
            state.projected_commits,
        ))
    })?;
    let branch = match from {
        Some(branch) => branch.to_owned(),
        None => original_git_ref.ok_or_else(|| {
            CliFailure::invalid(format!(
                "label {label} was not imported from Git; name the branch to pull with --from"
            ))
        })?,
    };

    let mut prepared = runtime.block_on(prepare_pull(
        &repository.objects,
        &repository.content,
        effective,
        projected_commits,
        &projection_path(&context.store_root)?,
        url,
        &branch,
        PullLimits::default(),
    ))?;
    let remote_tip = prepared.remote_tip.to_string();
    if prepared.commits.is_empty() {
        return Success::new(
            "pull",
            json!({
                "pulled": false,
                "label": label,
                "branch": branch,
                "remote_tip": remote_tip,
            }),
            format!("label {label} already holds {branch} at {remote_tip}\n"),
        );
    }

    let actor = authoring_metadata::actor(&context.store_root)?;
    let now = authoring_metadata::now()?;
    let objects = BatchObjectStore::new(&repository.objects, 1024, 8 * 1024 * 1024);
    let mutator = PersistedRepositoryMutator::new(&objects, &repository.content, &repository.heads)
        .with_application_limits(prepared.application_limits);
    let commits = std::mem::take(&mut prepared.commits);
    let (mutation, outcome) = runtime.block_on(async {
        let session = mutator
            .open_authoring(expected, listed.basis, effective)
            .await?;
        mutator
            .append_and_rebase(session, prepared.onto, commits, |old| RevisionMetadata {
                committer: actor.clone(),
                committed_at: now,
                provenance: None,
                ..old.metadata.clone()
            })
            .await
    })?;
    let publication = runtime.block_on(prepare_mutation_publication(
        &objects,
        &repository.content,
        &repository.heads,
        expected,
        mutation,
    ))?;
    runtime.block_on(objects.flush())?;
    let head = finalize_mutation_publication(&repository.heads, publication)?;
    // The projection is only read from here on.
    drop(prepared);

    let view = outcome.resulting_view;
    let tree = runtime
        .block_on(RepositoryReader::new(&repository.objects, &repository.content).view(view))
        .context("cannot read the pulled view")?
        .tree;
    let materialized = runtime.block_on(materialization::materialize_locked(
        &context.root,
        &repository,
        &mut context.workspace,
        head,
        label.clone(),
        listed.basis,
        (view != listed.basis).then_some(view),
        tree,
        dot_git,
    ));
    if let Err(error) = materialized {
        // The pull is recorded; only the workspace still shows the old view.
        return Err(error.context(format!(
            "the pulled commits are in view {view}, but the workspace was not switched to \
             it; run `wheat switch {label} --frontier {view}`"
        )));
    }

    let appended = outcome.appended.len();
    let mut human = format!(
        "pulled {appended} commit{} from {branch} at {remote_tip} into label {label}\n",
        if appended == 1 { "" } else { "s" }
    );
    let moved = outcome.rebased.len() + outcome.reapplied.len();
    if moved > 0 {
        writeln!(human, "moved {moved} local changes above them")?;
    }
    for change in &outcome.conflicted {
        writeln!(human, "conflicted {change}")?;
    }
    if !outcome.conflicted.is_empty() {
        writeln!(human, "see `wheat conflicts` to resolve them")?;
    }
    let ids =
        |ids: &mut dyn Iterator<Item = &ChangeId>| ids.map(ToString::to_string).collect::<Vec<_>>();
    Success::new(
        "pull",
        json!({
            "pulled": true,
            "label": label,
            "branch": branch,
            "remote_tip": remote_tip,
            "view": view.to_string(),
            "appended": ids(&mut outcome.appended.iter().map(|(change, _)| change)),
            "rebased": ids(&mut outcome.rebased.keys()),
            "reapplied": ids(&mut outcome.reapplied.iter()),
            "conflicted": ids(&mut outcome.conflicted.iter()),
        }),
        human,
    )
}
