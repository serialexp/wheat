use std::{
    fmt::Write as _,
    io::{BufRead as _, IsTerminal as _, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    str::FromStr,
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use wheat_core::{
    ApplicationResultRecord, ChangeId, ChangeProposal, ContentId, EditTransition, FileKind,
    FileValue, RevisionEdits, RevisionId, guarded::Limits,
};
use wheat_store::{
    ApplicationMapStore, BatchObjectStore, MerkleTreeStore, MutationTarget,
    PersistedConflictResolution, PersistedRepositoryMutator, PublicationStore, RepositoryReader,
    RevisionMapStore, finalize_mutation_publication, prepare_mutation_publication,
};
mod active_changes;
mod authoring_metadata;
mod command_output;
mod conflict_inspection;
mod diff_render;
mod discovery;
mod inspection;
mod local;
mod log_inspection;
mod materialization;
mod repository_diff;
mod selection_recovery;
mod workspace;
mod workspace_diff;
mod workspace_files;
mod workspace_metadata;
mod workspace_scan;
mod workspace_status;
use command_output::{
    CliFailure, OutputFormat, Success, clap_error, emit_error, emit_success, requested_format,
    require_supported_platform,
};
use discovery::{allocate_store_root, canonical_store_path, discover_workspace};
use local::{
    content_store, initialize, open, projection_path, repository_summary, revision_with_edits,
    runtime,
};
use workspace_metadata::{PendingSelection, StoreMetadata, WorkspaceMetadata, WorkspaceSelection};

#[derive(Debug, Parser)]
#[command(name = "wheat", about = "Wheat source control prototype")]
struct Cli {
    /// Explicit canonical Wheat store. Workspace discovery is used when omitted.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// Explicit workspace directory. Defaults to the current directory.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    /// Output format for people or automation.
    #[arg(long, value_enum, default_value_t, global = true)]
    format: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize an empty native repository and workspace.
    Init(InitArgs),
    /// Materialize the selected native view into the workspace.
    Materialize(MaterializeArgs),
    /// Restore tracked paths from the currently pinned materialized tree.
    Restore(RestoreArgs),
    /// Show repository and tracked workspace state.
    Status,
    /// Verify the complete retained repository state.
    Validate,
    /// Inspect logical changes and immutable revisions.
    Log,
    /// List changes active relative to this workspace's authoring basis.
    Changes,
    /// Inspect durable conflicts and per-edit failure evidence.
    Conflicts,
    /// Compare workspace or retained immutable repository trees.
    Diff(DiffArgs),
    /// Read or write typed Wheat configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Inspect immutable edit definitions and lineage.
    Edits {
        #[arg(long)]
        revision: String,
    },
    /// Add explicitly selected workspace paths as a new revision.
    Add(AddArgs),
    /// Print retained content by its native identity.
    Cat { content: String },
    /// Create a logical change with its initial captured edit.
    #[command(subcommand)]
    Change(ChangeCommand),
    /// Resolve a durable conflict with an exact tree entry.
    Resolve(ResolveArgs),
    /// Import a local Git repository into a fresh Wheat repository.
    Import(ImportArgs),
    /// Project Wheat revision history and push it to a Git branch.
    Push(PushArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Workspace to initialize. Defaults to the current directory.
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct MaterializeArgs {
    /// Wheat source: published, working, or branch:<name>.
    #[arg(long, default_value = "published")]
    source: String,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    /// Tracked leaf or directory paths in the currently pinned tree.
    #[arg(required = true)]
    paths: Vec<String>,
}

#[derive(Debug, Args)]
struct DiffArgs {
    #[arg(long, conflicts_with_all = ["from", "to"])]
    revision: Option<String>,
    #[arg(long, requires = "to", conflicts_with = "revision")]
    from: Option<String>,
    #[arg(long, requires = "from", conflicts_with = "revision")]
    to: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Set one typed configuration key.
    Set {
        /// Write user configuration instead of the current repository configuration.
        #[arg(long)]
        global: bool,
        /// Configuration key: actor.name or actor.email.
        key: authoring_metadata::ConfigKey,
        value: String,
    },
}

#[derive(Debug, Subcommand)]
enum ChangeCommand {
    /// Create a logical change and select it in this workspace.
    New(Box<NewArgs>),
    /// Select an existing active change for subsequent `wheat add` commands.
    Select { change: String },
}

#[derive(Debug, Args)]
struct NewArgs {
    /// Logical changes this change depends on. Omit to build on the current working frontier.
    #[arg(long = "depends-on", conflicts_with = "independent")]
    dependencies: Vec<String>,
    /// Create a root change against the repository base instead of the current working frontier.
    #[arg(long)]
    independent: bool,
    #[command(flatten)]
    metadata: NewMetadataArgs,
    #[command(flatten)]
    edit: EditArgs,
}

#[derive(Debug, Args)]
struct NewMetadataArgs {
    /// Native revision message. Its first nonempty line is the stable change title.
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Stable logical-change description.
    #[arg(long)]
    description: Option<String>,
    /// Creator timestamp as RFC3339. Defaults to the current local time.
    #[arg(long)]
    created_at: Option<String>,
    /// Author timestamp as RFC3339. Defaults to the current local time.
    #[arg(long)]
    authored_at: Option<String>,
    /// Committer timestamp as RFC3339. Defaults to the current local time.
    #[arg(long)]
    committed_at: Option<String>,
}

#[derive(Debug, Args)]
struct RevisionMetadataArgs {
    /// Optional note for this immutable revision.
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Author timestamp as RFC3339. Defaults to the current local time.
    #[arg(long)]
    authored_at: Option<String>,
    /// Committer timestamp as RFC3339. Defaults to the current local time.
    #[arg(long)]
    committed_at: Option<String>,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Active logical change to revise instead of the workspace selection.
    #[arg(long)]
    change: Option<String>,
    /// Advanced stale-state assertion retained for low-level automation.
    #[arg(long, hide = true)]
    predecessor: Option<String>,
    /// Explicit repository-relative workspace paths. No other files are read or written.
    #[arg(required = true)]
    paths: Vec<String>,
    #[command(flatten)]
    metadata: RevisionMetadataArgs,
    /// Wider guard as PATH:START:END, using byte offsets in canonical-LF workspace content.
    #[arg(long = "guard", value_parser = parse_guard)]
    guards: Vec<GuardRange>,
}

#[derive(Clone, Debug)]
struct GuardRange {
    path: String,
    range: std::ops::Range<usize>,
}

fn change_message(value: Option<String>, format: OutputFormat) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if format == OutputFormat::Json {
        return Err(CliFailure::invalid(
            "change message is required for JSON output; pass --message <message>",
        ));
    }
    if !std::io::stdin().is_terminal() {
        return Err(CliFailure::invalid(
            "change message is required in non-interactive use; pass --message <message>",
        ));
    }
    eprint!("Change message: ");
    std::io::stderr().flush()?;
    let mut value = String::new();
    std::io::stdin()
        .lock()
        .take(1024 * 1024 + 1)
        .read_line(&mut value)?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(value)
}

fn parse_guard(value: &str) -> std::result::Result<GuardRange, String> {
    let (prefix, end) = value.rsplit_once(':').ok_or("expected PATH:START:END")?;
    let (path, start) = prefix.rsplit_once(':').ok_or("expected PATH:START:END")?;
    let start = start
        .parse()
        .map_err(|_| "START must be an unsigned byte offset")?;
    let end = end
        .parse()
        .map_err(|_| "END must be an unsigned byte offset")?;
    Ok(GuardRange {
        path: path.to_owned(),
        range: start..end,
    })
}

#[derive(Debug, Args)]
struct EditArgs {
    /// Repository-relative path to edit.
    #[arg(long)]
    path: Option<String>,
    /// Expected current UTF-8 content; omit when creating a path.
    #[arg(long)]
    expected: Option<String>,
    /// Expected file kind (defaults to regular).
    #[arg(long, value_enum)]
    expected_kind: Option<CliFileKind>,
    /// Replacement UTF-8 content; omit with --delete or for a mode-only edit.
    #[arg(long, conflicts_with = "delete")]
    value: Option<String>,
    /// Replacement file kind (defaults to the expected kind for an existing path, regular for a new path).
    #[arg(long, value_enum)]
    replacement_kind: Option<CliFileKind>,
    /// Delete the path.
    #[arg(long)]
    delete: bool,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum CliFileKind {
    #[default]
    Regular,
    Executable,
    Symlink,
}

impl From<CliFileKind> for FileKind {
    fn from(value: CliFileKind) -> Self {
        match value {
            CliFileKind::Regular => Self::Regular,
            CliFileKind::Executable => Self::Executable,
            CliFileKind::Symlink => Self::Symlink,
        }
    }
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// Local Git repository path or remote Git URL to import.
    source: String,
    /// Fresh workspace to create. Defaults to the current directory.
    workspace: Option<PathBuf>,
    /// Full Git branch ref to select as Wheat's working and published view.
    #[arg(long, default_value = "refs/heads/main")]
    branch: String,
}

#[derive(Debug, Args)]
struct PushArgs {
    /// Git remote URL. It is passed to Git and never persisted by Wheat.
    remote: String,
    /// Full destination branch ref, for example refs/heads/main.
    #[arg(long)]
    to: String,
    /// Wheat source: published, working, or branch:<name>.
    #[arg(long, default_value = "published")]
    source: String,
    /// Replace an unknown/diverged destination only if it still has the observed OID.
    #[arg(long)]
    force_with_lease: bool,
}

#[derive(Debug, Args)]
struct ResolveArgs {
    /// Conflicted immutable revision (advanced disambiguation; inferred from --path when omitted).
    revision: Option<String>,
    /// Path whose resolved value should be recorded.
    #[arg(long)]
    path: String,
    /// Resolved UTF-8 content; omit with --delete.
    #[arg(long, conflicts_with = "delete", required_unless_present = "delete")]
    value: Option<String>,
    /// Resolved file kind (defaults to the target tree's existing kind, or regular for a new path).
    #[arg(long, value_enum, conflicts_with = "delete")]
    kind: Option<CliFileKind>,
    /// Resolve by deleting the path.
    #[arg(long)]
    delete: bool,
}

fn main() -> ExitCode {
    let args = std::env::args_os().collect::<Vec<_>>();
    let requested = requested_format(&args);
    if let Err(error) = require_supported_platform() {
        return emit_error(requested, "cli", &error);
    }
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return clap_error(requested, error),
    };
    let format = cli.format;
    let command = command_name(&cli.command);
    match execute(cli) {
        Ok(success) => emit_success(format, &success),
        Err(error) => emit_error(format, command, &error),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init(_) => "init",
        Command::Materialize(_) => "materialize",
        Command::Restore(_) => "restore",
        Command::Status => "status",
        Command::Validate => "validate",
        Command::Log => "log",
        Command::Changes => "changes",
        Command::Conflicts => "conflicts",
        Command::Diff(_) => "diff",
        Command::Config(ConfigCommand::Set { .. }) => "config.set",
        Command::Edits { .. } => "edits",
        Command::Add(_) => "add",
        Command::Cat { .. } => "cat",
        Command::Change(ChangeCommand::New(_)) => "change.new",
        Command::Change(ChangeCommand::Select { .. }) => "change.select",
        Command::Resolve(_) => "resolve",
        Command::Import(_) => "import",
        Command::Push(_) => "push",
    }
}

fn execute(cli: Cli) -> Result<Success> {
    let format = cli.format;
    let requested_workspace = cli.workspace.unwrap_or_else(|| PathBuf::from("."));
    match cli.command {
        Command::Init(arguments) => init_workspace(
            arguments
                .workspace
                .as_deref()
                .unwrap_or(&requested_workspace),
            cli.store.as_deref(),
        ),
        Command::Materialize(arguments) => {
            let (view, root) = materialize_workspace(
                &requested_workspace,
                cli.store.as_deref(),
                &arguments.source,
            )?;
            Success::new(
                "materialize",
                json!({ "view": view.to_string() }),
                format!("materialized view {view} in {}\n", root.display()),
            )
        }
        Command::Restore(arguments) => {
            let restored =
                restore_workspace(&requested_workspace, cli.store.as_deref(), &arguments.paths)?;
            Success::new(
                "restore",
                json!({ "restored": restored }),
                format!("restored {restored} tracked path(s)\n"),
            )
        }
        Command::Status => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let summary = repository_summary(&context.store_root)?;
            let local = open(&context.store_root)?;
            let status = workspace_status::WorkspaceStatus::from(workspace_scan::scan(
                &context.root,
                &local,
                &context.workspace,
                0,
            )?);
            let mut output = String::new();
            writeln!(output, "working view: {}", summary.working_view)?;
            writeln!(output, "published view: {}", summary.published_view)?;
            writeln!(output, "changes: {}", summary.changes)?;
            writeln!(output, "conflicts: {}", summary.conflicts)?;
            writeln!(output, "tracked changes: {}", status.changes.len())?;
            if status.authority_advanced {
                writeln!(output, "repository advanced since materialization")?;
            }
            for change in &status.changes {
                let kind = if change.deleted {
                    "deleted"
                } else if change.content_changed && change.kind_changed {
                    "modified+kind"
                } else if change.content_changed {
                    "modified"
                } else {
                    "kind"
                };
                writeln!(output, "{kind} {}", change.path)?;
            }
            Success::new("status", &status, output)
        }
        Command::Validate => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let local = open(&store_root)?;
            let head = local.heads.read()?;
            let runtime = runtime()?;
            let state = runtime.block_on(async {
                let reader = RepositoryReader::new(&local.objects, &local.content);
                let envelope = reader.envelope(head.envelope).await?;
                reader.validate_complete(envelope.state).await?;
                Ok::<_, wheat_store::RepositoryPublicationError>(envelope.state)
            })?;
            Success::new(
                "validate",
                json!({ "state": state.to_string() }),
                format!("valid state: {state}\n"),
            )
        }
        Command::Log => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            log_inspection::inspect(&store_root, format)
        }
        Command::Changes => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let basis = context
                .workspace
                .authoring_basis
                .context("workspace has no authoring basis; materialize it first")?;
            let head = local.heads.read()?;
            let document = runtime()?.block_on(active_changes::inspect(
                &local,
                head,
                basis,
                context.workspace.selected_change,
            ))?;
            let human = active_changes::render_human(&document)?;
            Success::new("changes", &document, human)
        }
        Command::Conflicts => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let local = open(&store_root)?;
            let document = runtime()?.block_on(conflict_inspection::inspect(&local))?;
            let mut human = Vec::new();
            conflict_inspection::human(&document, &mut human)?;
            Success::new("conflicts", &document, human)
        }
        Command::Diff(arguments) => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let limits = wheat_core::diff_model::DiffLimits::default();
            let deadline = std::time::Instant::now() + limits.deadline;
            let document = if let Some(revision) = arguments.revision {
                let revision = RevisionId::from_str(&revision)
                    .map_err(|_| CliFailure::invalid("invalid revision ID"))?;
                let input =
                    runtime()?.block_on(repository_diff::revision(&local, revision, deadline))?;
                let comparison = match input.comparison {
                    repository_diff::ComparisonInput::Revision { revision, from, to } => {
                        wheat_core::diff_model::DiffComparison::Revision {
                            revision,
                            from_tree: from,
                            to_tree: to,
                            head_generation: input.head_generation,
                        }
                    }
                    _ => unreachable!("revision resolver returns revision comparison"),
                };
                workspace_diff::build_repository(comparison, input.changes, limits, deadline)?
            } else if let (Some(from), Some(to)) = (arguments.from, arguments.to) {
                let input =
                    runtime()?.block_on(repository_diff::views(&local, &from, &to, deadline))?;
                let comparison = match input.comparison {
                    repository_diff::ComparisonInput::Views {
                        from_view,
                        to_view,
                        from,
                        to,
                    } => wheat_core::diff_model::DiffComparison::Views {
                        from_view,
                        to_view,
                        from_tree: from,
                        to_tree: to,
                        head_generation: input.head_generation,
                    },
                    _ => unreachable!("view resolver returns view comparison"),
                };
                workspace_diff::build_repository(comparison, input.changes, limits, deadline)?
            } else {
                let scan = workspace_scan::scan(
                    &context.root,
                    &local,
                    &context.workspace,
                    limits.max_inline_bytes_per_side,
                )?;
                workspace_diff::build(scan, limits, deadline)?
            };
            let human = diff_render::human_bytes(&document)?;
            Success::new("diff", &document, human)
        }
        Command::Config(ConfigCommand::Set { global, key, value }) => {
            let store_root = if global {
                None
            } else {
                Some(resolve_store_root(
                    &requested_workspace,
                    cli.store.as_deref(),
                )?)
            };
            let key_name = match key {
                authoring_metadata::ConfigKey::ActorName => "actor.name",
                authoring_metadata::ConfigKey::ActorEmail => "actor.email",
            };
            authoring_metadata::set(store_root.as_deref(), global, key, &value)?;
            Success::new(
                "config.set",
                json!({ "global": global, "key": key_name }),
                Vec::new(),
            )
        }
        Command::Edits { revision } => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let revision = RevisionId::from_str(&revision)
                .map_err(|_| CliFailure::invalid("invalid revision ID"))?;
            let (revision, edits) = revision_with_edits(&store_root, revision)?;
            let mut output = String::new();
            inspection::edits(&revision, &edits, &mut output)?;
            Success::new(
                "edits",
                json!({ "revision": revision.id().to_string(), "text": output }),
                output.clone(),
            )
        }
        Command::Add(arguments) => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            // Recovery, capture, and publication are one workspace operation:
            // otherwise pending or concurrent materialization can rewrite a selected
            // leaf between capture and the repository-head compare-and-swap.
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            let runtime = runtime()?;
            runtime.block_on(materialization::recover(
                &context.root,
                &local,
                &mut context.workspace,
            ))?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let expected = local.heads.read()?;
            let basis = context
                .workspace
                .authoring_basis
                .context("workspace has no authoring basis; materialize it first")?;
            let active = runtime.block_on(active_changes::inspect(
                &local,
                expected,
                basis,
                context.workspace.selected_change,
            ))?;
            let change = arguments
                .change
                .as_deref()
                .map(ChangeId::from_str)
                .transpose()
                .context("invalid change ID")?
                .or(context.workspace.selected_change)
                .context(
                    "workspace has no selected change; run `wheat change new` or pass --change",
                )?;
            let destination = active
                .changes
                .iter()
                .find(|candidate| candidate.change == change)
                .with_context(|| format!("change {change} is not active in this workspace"))?;
            let predecessor = arguments
                .predecessor
                .as_deref()
                .map(RevisionId::from_str)
                .transpose()
                .context("invalid predecessor revision ID")?
                .unwrap_or(destination.revision);
            let title = destination.title.clone();
            let mut guards = std::collections::BTreeMap::new();
            for guard in arguments.guards {
                guards
                    .entry(guard.path)
                    .or_insert_with(Vec::new)
                    .push(guard.range);
            }
            let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
            let edits = runtime.block_on(workspace::author_from_workspace(
                &context.root,
                &objects,
                &local.content,
                expected,
                change,
                predecessor,
                &arguments.paths,
                &guards,
            ))?;
            let (_, revision_metadata) = authoring_metadata::metadata(
                &context.store_root,
                authoring_metadata::MetadataArgs {
                    message: arguments.metadata.message.unwrap_or_default(),
                    description: None,
                    created_at: None,
                    authored_at: arguments.metadata.authored_at,
                    committed_at: arguments.metadata.committed_at,
                },
                false,
            )?;
            let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads)
                .with_target(MutationTarget::PublishWorking);
            let (prepared, outcome) = runtime.block_on(mutator.revise(
                expected,
                change,
                predecessor,
                revision_metadata,
                edits,
            ))?;
            runtime.block_on(async {
                let publication = prepare_mutation_publication(
                    &objects,
                    &local.content,
                    &local.heads,
                    expected,
                    prepared,
                )
                .await?;
                objects.flush().await?;
                finalize_mutation_publication(&local.heads, publication)
            })?;
            let mut output = String::new();
            writeln!(
                output,
                "added {} path(s) to change {change} — {title}",
                arguments.paths.len()
            )?;
            writeln!(output, "revision {}", outcome.revision_id)?;
            if arguments.change.is_some()
                && context.workspace.selected_change != Some(change)
                && let Some(selected) = context.workspace.selected_change
            {
                writeln!(output, "selected change remains {selected}")?;
            }
            print_reapplied(&mut output, &outcome.reapplied)?;
            Success::new(
                "add",
                json!({
                    "change": change.to_string(),
                    "revision": outcome.revision_id.to_string(),
                    "paths": arguments.paths.len(),
                    "reapplied": outcome.reapplied.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
                output,
            )
        }
        Command::Cat { content } => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let id = ContentId::from_str(&content)
                .map_err(|_| CliFailure::invalid("invalid content ID"))?;
            let store = content_store(&store_root)?;
            let limit = if format == OutputFormat::Json {
                command_output::MAX_OUTPUT_BYTES / 4
            } else {
                command_output::MAX_OUTPUT_BYTES
            };
            let bytes = store.get_bounded(id, limit)?;
            let result = json!({ "content_id": id.to_string(), "bytes": &bytes });
            Success::raw("cat", result, bytes)
        }
        Command::Change(ChangeCommand::New(arguments)) => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            let runtime = runtime()?;
            runtime.block_on(materialization::recover(
                &context.root,
                &local,
                &mut context.workspace,
            ))?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let expected = local.heads.read()?;
            let dependencies = arguments
                .dependencies
                .iter()
                .map(|value| ChangeId::from_str(value).context("invalid dependency change ID"))
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            let use_frontier = dependencies.is_empty() && !arguments.independent;
            let message = change_message(arguments.metadata.message, format)?;
            let (change_metadata, revision_metadata) = authoring_metadata::metadata(
                &context.store_root,
                authoring_metadata::MetadataArgs {
                    message,
                    description: arguments.metadata.description,
                    created_at: arguments.metadata.created_at,
                    authored_at: arguments.metadata.authored_at,
                    committed_at: arguments.metadata.committed_at,
                },
                true,
            )?;
            let edit = arguments.edit;
            let has_edit = edit.path.is_some()
                || edit.expected.is_some()
                || edit.expected_kind.is_some()
                || edit.value.is_some()
                || edit.replacement_kind.is_some()
                || edit.delete;
            let edits = if has_edit {
                capture_initial_edit(&local.content, edit)?
            } else {
                RevisionEdits::default()
            };
            let change = ChangeId::new();
            context.workspace.pending_selection = Some(PendingSelection {
                change,
                expected_head: expected.into(),
            });
            context
                .workspace
                .replace_prepared(&context.root)?
                .publish()
                .map_err(|error| anyhow::anyhow!(error))?;
            let proposal = ChangeProposal {
                id: change,
                metadata: change_metadata.expect("new change metadata requested"),
                dependencies,
                revision_metadata,
                edits,
            };
            let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
            let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads)
                .with_target(MutationTarget::PublishWorking);
            let (prepared, outcome) = if use_frontier {
                runtime.block_on(mutator.create_on_frontier(expected, proposal))?
            } else {
                runtime.block_on(mutator.create(expected, proposal))?
            };
            runtime.block_on(async {
                let publication = prepare_mutation_publication(
                    &objects,
                    &local.content,
                    &local.heads,
                    expected,
                    prepared,
                )
                .await?;
                objects.flush().await?;
                finalize_mutation_publication(&local.heads, publication)
            })?;
            context.workspace.selected_change = Some(change);
            context.workspace.pending_selection = None;
            context
                .workspace
                .replace_prepared(&context.root)?
                .publish()
                .map_err(|error| anyhow::anyhow!(error))?;
            let mut output = String::new();
            writeln!(output, "change {change}")?;
            writeln!(output, "revision {}", outcome.revision_id)?;
            print_reapplied(&mut output, &outcome.reapplied)?;
            Success::new(
                "change.new",
                json!({
                    "change": change.to_string(),
                    "revision": outcome.revision_id.to_string(),
                    "reapplied": outcome.reapplied.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
                output,
            )
        }
        Command::Change(ChangeCommand::Select { change }) => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let change = ChangeId::from_str(&change)
                .map_err(|_| CliFailure::invalid("invalid change ID"))?;
            let local = open(&context.store_root)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let basis = context
                .workspace
                .authoring_basis
                .context("workspace has no authoring basis; materialize it first")?;
            let head = local.heads.read()?;
            let active = runtime()?.block_on(active_changes::inspect(
                &local,
                head,
                basis,
                context.workspace.selected_change,
            ))?;
            let selected = active
                .changes
                .iter()
                .find(|candidate| candidate.change == change)
                .with_context(|| format!("change {change} is not active in this workspace"))?;
            context.workspace.selected_change = Some(change);
            context
                .workspace
                .replace_prepared(&context.root)?
                .publish()
                .map_err(|error| anyhow::anyhow!(error))?;
            Success::new(
                "change.select",
                json!({ "change": change.to_string(), "title": selected.title }),
                format!("selected change {change} — {}\n", selected.title),
            )
        }
        Command::Resolve(arguments) => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            resolve(&store_root, arguments)
        }
        Command::Import(arguments) => import(arguments, cli.store.as_deref(), &requested_workspace),
        Command::Push(arguments) => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            push(&store_root, arguments)
        }
    }
}

fn import(
    arguments: ImportArgs,
    explicit_store: Option<&Path>,
    default_workspace: &Path,
) -> Result<Success> {
    let workspace_root = arguments.workspace.as_deref().unwrap_or(default_workspace);
    ensure_fresh_workspace(workspace_root)?;
    let store_root = explicit_store
        .map(Path::to_path_buf)
        .unwrap_or(allocate_store_root()?);
    if store_root.exists() {
        return Err(CliFailure::invalid(format!(
            "Wheat store destination already exists: {}",
            store_root.display()
        )));
    }
    let local_source = Path::new(&arguments.source);
    let clone;
    let source = if local_source.exists() {
        local_source
    } else {
        clone = clone_remote_for_import(&arguments.source)?;
        clone.path()
    };
    let runtime = runtime()?;
    let head = runtime.block_on(wheat_git::atomic_import(
        source,
        &store_root,
        &arguments.branch,
    ))?;
    let store = StoreMetadata::create(&store_root)?;
    let canonical_store = canonical_store_path(&store_root)?;
    let workspace = WorkspaceMetadata::new(
        canonical_store,
        store.store_id,
        WorkspaceSelection::Branch {
            name: arguments.branch.clone(),
        },
    );
    initialize_workspace_directory(workspace_root)?;
    workspace.initialize(workspace_root)?;
    let (view, materialized_root) = materialize_workspace(
        workspace_root,
        Some(&store_root),
        &format!("branch:{}", arguments.branch),
    )?;
    let human = format!(
        "materialized view {view} in {}\nimported {} at generation {}\nenvelope {}\n",
        materialized_root.display(),
        arguments.branch,
        head.generation,
        head.envelope,
    );
    Success::new(
        "import",
        json!({
            "branch": arguments.branch,
            "generation": head.generation,
            "envelope": head.envelope.to_string(),
        }),
        human,
    )
}

fn init_workspace(workspace_root: &Path, explicit_store: Option<&Path>) -> Result<Success> {
    ensure_fresh_workspace(workspace_root)?;
    let store_root = explicit_store
        .map(Path::to_path_buf)
        .unwrap_or(allocate_store_root()?);
    initialize(&store_root)?;
    let store = StoreMetadata::create(&store_root)?;
    let canonical_store = canonical_store_path(&store_root)?;
    initialize_workspace_directory(workspace_root)?;
    WorkspaceMetadata::new(
        canonical_store,
        store.store_id,
        WorkspaceSelection::Published,
    )
    .initialize(workspace_root)?;
    let (view, materialized_root) =
        materialize_workspace(workspace_root, Some(&store_root), "published")?;
    Success::new(
        "init",
        json!({ "view": view.to_string() }),
        format!(
            "materialized view {view} in {}\ninitialized Wheat workspace at {}\nstore {}\n",
            materialized_root.display(),
            workspace_root.display(),
            store_root.display(),
        ),
    )
}

fn ensure_fresh_workspace(root: &Path) -> Result<()> {
    if root.exists() && root.read_dir()?.next().is_some() {
        return Err(CliFailure::invalid(format!(
            "Wheat workspace destination is not empty: {}",
            root.display()
        )));
    }
    Ok(())
}

fn initialize_workspace_directory(root: &Path) -> Result<()> {
    if !root.exists() {
        std::fs::create_dir_all(root)?;
    }
    Ok(())
}

fn resolve_context(
    workspace: &Path,
    explicit_store: Option<&Path>,
) -> Result<discovery::ResolvedWorkspace> {
    let context = discover_workspace(workspace)?;
    if let Some(store) = explicit_store {
        let explicit = canonical_store_path(store)?;
        if explicit != context.store_root {
            return Err(CliFailure::invalid(
                "explicit Wheat store does not match the discovered workspace link",
            ));
        }
    }
    Ok(context)
}

fn resolve_store_root(workspace: &Path, explicit_store: Option<&Path>) -> Result<PathBuf> {
    if let Some(store) = explicit_store {
        let store = canonical_store_path(store)?;
        StoreMetadata::read(&store)?;
        return Ok(store);
    }
    Ok(discover_workspace(workspace)?.store_root)
}

fn materialize_workspace(
    workspace: &Path,
    explicit_store: Option<&Path>,
    source: &str,
) -> Result<(wheat_core::ViewId, PathBuf)> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let requested_selection = parse_selection(source)?;
    let repository = open(&context.store_root)?;
    let head = repository.heads.read()?;
    let runtime = runtime()?;
    let (view, tree) = runtime.block_on(resolve_selected_view(
        &repository,
        head,
        &requested_selection,
    ))?;
    runtime.block_on(materialization::materialize(
        &context.root,
        &repository,
        &mut context.workspace,
        head,
        view,
        tree,
        requested_selection,
    ))?;
    Ok((view, context.root))
}

fn restore_workspace(
    workspace: &Path,
    explicit_store: Option<&Path>,
    paths: &[String],
) -> Result<usize> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let restored = runtime()?.block_on(materialization::restore(
        &context.root,
        &repository,
        &mut context.workspace,
        paths,
    ))?;
    Ok(restored)
}

fn parse_selection(source: &str) -> Result<WorkspaceSelection> {
    match source {
        "published" => Ok(WorkspaceSelection::Published),
        "working" => Ok(WorkspaceSelection::Working),
        value if value.starts_with("branch:") => Ok(WorkspaceSelection::Branch {
            name: value
                .strip_prefix("branch:")
                .expect("matched prefix")
                .into(),
        }),
        _ => Err(CliFailure::invalid(
            "source must be published, working, or branch:<name>",
        )),
    }
}

async fn resolve_selected_view(
    repository: &local::LocalRepository,
    head: wheat_store::RepositoryHead,
    selection: &WorkspaceSelection,
) -> Result<(wheat_core::ViewId, wheat_core::TreeId)> {
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    let envelope = reader.validate_envelope(head.envelope).await?;
    let state = reader.state(envelope.state).await?;
    let view = match selection {
        WorkspaceSelection::Published => state.published_view,
        WorkspaceSelection::Working => state.working_view,
        WorkspaceSelection::Branch { name } => {
            let branches = reader.branch_manifest(envelope.branches).await?;
            *branches
                .branches
                .get(name)
                .with_context(|| format!("unknown Wheat branch {name}"))?
        }
    };
    Ok((view, reader.view(view).await?.tree))
}

fn clone_remote_for_import(remote: &str) -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir().context("create temporary Git clone directory")?;
    let status = ProcessCommand::new("git")
        .arg("clone")
        .arg("--mirror")
        .arg("--")
        .arg(remote)
        .arg(directory.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawn git clone for Wheat import")?;
    if !status.success() {
        return Err(CliFailure::transport(format!(
            "git clone for Wheat import failed with status {:?}",
            status.code()
        )));
    }
    Ok(directory)
}

fn push(root: &std::path::Path, arguments: PushArgs) -> Result<Success> {
    let local = open(root)?;
    let expected = local.heads.read()?;
    let runtime = runtime()?;
    let (base, selected) = runtime.block_on(async {
        let reader = RepositoryReader::new(&local.objects, &local.content);
        let envelope = reader.validate_envelope(expected.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let selected = match arguments.source.as_str() {
            "published" => state.published_view,
            "working" => state.working_view,
            source if source.starts_with("branch:") => {
                let name = source.strip_prefix("branch:").expect("matched prefix");
                let branches = reader.branch_manifest(envelope.branches).await?;
                *branches
                    .branches
                    .get(name)
                    .with_context(|| format!("unknown Wheat branch {name}"))?
            }
            _ => {
                return Err(CliFailure::invalid(
                    "source must be published, working, or branch:<name>",
                ));
            }
        };
        Ok::<_, anyhow::Error>((state.base_view, selected))
    })?;
    let prepared = runtime.block_on(wheat_git::bridge::prepare_push(
        &local.objects,
        &local.content,
        selected,
        base,
        &projection_path(root)?,
        &arguments.remote,
        &arguments.to,
        arguments.force_with_lease,
    ))?;
    let actual = local.heads.read()?;
    if actual != expected {
        return Err(CliFailure::stale(
            "Wheat repository changed while preparing the Git projection; retry the push",
        ));
    }
    let projected = prepared
        .export
        .projected
        .iter()
        .filter(|application| !application.reused_anchor)
        .count();
    let tip = prepared
        .export
        .tip
        .context("projection did not produce a Git commit")?;
    let report = wheat_git::bridge::publish_prepared(prepared)?;
    let outcome = report.refs.first().context("git returned no ref status")?;
    let up_to_date = matches!(outcome.status, wheat_git::push::RefStatus::UpToDate);
    let human = if up_to_date {
        format!("{} is already at {tip}\n", arguments.to)
    } else {
        format!(
            "pushed {projected} Wheat revisions to {} at {tip}\n",
            arguments.to
        )
    };
    Success::new(
        "push",
        json!({
            "destination": arguments.to,
            "tip": tip.to_string(),
            "projected_revisions": projected,
            "up_to_date": up_to_date,
        }),
        human,
    )
}

fn resolve(root: &std::path::Path, arguments: ResolveArgs) -> Result<Success> {
    if arguments.path.len() > conflict_inspection::MAX_PATH_BYTES {
        return Err(CliFailure::limit(
            "resolution path exceeds conflict inspection limit",
        ));
    }
    let local = open(root)?;
    let runtime = runtime()?;
    let revision = if let Some(revision) = arguments.revision.as_deref() {
        RevisionId::from_str(revision).map_err(|_| CliFailure::invalid("invalid revision ID"))?
    } else {
        let document = runtime.block_on(conflict_inspection::inspect(&local))?;
        let mut matches = std::collections::BTreeSet::new();
        for conflict in document.conflicts {
            if conflict
                .edits
                .iter()
                .any(|edit| edit.path == arguments.path)
            {
                matches.insert(conflict.revision_id);
            }
        }
        match matches.len() {
            1 => matches.into_iter().next().expect("one matching revision"),
            0 => {
                return Err(CliFailure::conflict(
                    "no current conflicted revision affects the requested path",
                ));
            }
            _ => {
                return Err(CliFailure::conflict(
                    "multiple current conflicted revisions affect the requested path; specify a revision ID",
                ));
            }
        }
    };
    let expected = local.heads.read()?;
    let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
    let reader = RepositoryReader::new(&objects, &local.content);
    let (application_id, existing) = runtime.block_on(async {
        let envelope = reader.envelope(expected.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let working = reader.view(state.working_view).await?;
        let record = reader.revision(revision).await?;
        let current = RevisionMapStore::new(&objects)
            .get(working.revisions, record.change_id)
            .await?;
        if current != Some(revision) {
            return Err(CliFailure::conflict(
                "revision is not the current conflicted application",
            ));
        }
        let application_id = ApplicationMapStore::new(&objects)
            .get(working.applications, record.change_id)
            .await?
            .context("revision is not the current conflicted application")?;
        let application = reader.application(application_id).await?;
        if application.revision_id != revision
            || !matches!(
                application.result,
                ApplicationResultRecord::Conflicted { .. }
            )
        {
            return Err(CliFailure::conflict(
                "revision is not the current conflicted application",
            ));
        }
        let target = reader.view(application.target_view).await?;
        let components = arguments
            .path
            .split('/')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let existing = MerkleTreeStore::new(&objects)
            .lookup(target.tree, &components)
            .await?;
        Ok::<_, anyhow::Error>((application_id, existing))
    })?;
    // Store/authenticate content before metadata referring to it is prepared.
    let value = match arguments.value {
        Some(value) => {
            let kind = arguments
                .kind
                .map(Into::into)
                .or_else(|| existing.map(|value| value.kind))
                .unwrap_or(FileKind::Regular);
            let content = put_captured_value(&local.content, &value, kind)?;
            Some(FileValue { content, kind })
        }
        None if arguments.delete => None,
        None => return Err(CliFailure::invalid("a value or --delete is required")),
    };
    let resolution = PersistedConflictResolution {
        conflict_revision: revision,
        application: application_id,
        resolved: std::collections::BTreeMap::from([(arguments.path, value)]),
    };
    let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads)
        .with_target(MutationTarget::PublishWorking);
    let (prepared, outcome) = runtime.block_on(mutator.resolve_conflict(expected, resolution))?;
    runtime.block_on(async {
        let publication = prepare_mutation_publication(
            &objects,
            &local.content,
            &local.heads,
            expected,
            prepared,
        )
        .await?;
        objects.flush().await?;
        finalize_mutation_publication(&local.heads, publication)
    })?;
    let mut output = String::new();
    writeln!(output, "revision {}", outcome.revision_id)?;
    print_reapplied(&mut output, &outcome.reapplied)?;
    Success::new(
        "resolve",
        json!({
            "revision": outcome.revision_id.to_string(),
            "reapplied": outcome.reapplied.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }),
        output,
    )
}

/// Captures the initial whole-file edit for `change new`.
fn capture_initial_edit(
    store: &wheat_store::ContentStore,
    arguments: EditArgs,
) -> Result<RevisionEdits> {
    let expected_kind: FileKind = arguments.expected_kind.unwrap_or_default().into();
    let replacement_kind = arguments
        .replacement_kind
        .map(Into::into)
        .unwrap_or(expected_kind);
    let before =
        arguments
            .expected
            .as_deref()
            .map(|bytes| wheat_core::edit_capture::FileSnapshot {
                kind: expected_kind,
                bytes: bytes.as_bytes(),
            });
    let after = (!arguments.delete)
        .then(|| arguments.value.as_deref().or(arguments.expected.as_deref()))
        .flatten()
        .map(|bytes| wheat_core::edit_capture::FileSnapshot {
            kind: replacement_kind,
            bytes: bytes.as_bytes(),
        });
    let path = arguments
        .path
        .context("--path is required when supplying an initial edit")?;
    if !arguments.delete && arguments.value.is_none() && arguments.replacement_kind.is_none() {
        return Err(CliFailure::invalid(
            "--value, --replacement-kind, or --delete is required for an initial edit",
        ));
    }
    let captured =
        wheat_core::edit_capture::capture_edit(&path, before, after, None, Limits::default())?;
    workspace::persist_content(store, captured.content)?;
    Ok(RevisionEdits {
        transition: EditTransition {
            added: captured.edits.keys().copied().collect(),
            ..Default::default()
        },
        edits: captured.edits,
    })
}

fn put_captured_value(
    store: &wheat_store::ContentStore,
    value: &str,
    kind: FileKind,
) -> Result<ContentId> {
    if kind == FileKind::Symlink {
        return Ok(store.put(value.as_bytes())?);
    }
    let captured = wheat_core::text_capture::capture(value.as_bytes());
    let bytes = match &captured {
        wheat_core::text_capture::CapturedBytes::Text(value) => value.as_bytes(),
        wheat_core::text_capture::CapturedBytes::Binary(value) => value.as_ref(),
    };
    Ok(store.put(bytes)?)
}

fn print_reapplied(output: &mut String, changes: &[ChangeId]) -> Result<()> {
    for change in changes {
        writeln!(output, "reapplied {change}")?;
    }
    Ok(())
}
