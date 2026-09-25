use std::{
    fmt::Write as _,
    io::{BufRead as _, IsTerminal as _, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use wheat_core::{
    ApplicationResultRecord, ChangeId, ChangeProposal, ContentId, EditFailure, EditTransition,
    FileKind, FileValue, RepositoryError, RevisionEdits, RevisionId, ViewId, guarded::Limits,
};
use wheat_store::{
    ApplicationMapStore, BatchObjectStore, MerkleTreeStore, PersistedConflictResolution,
    PersistedRepositoryMutator, PublicationStore, RepositoryReader, RevisionMapStore,
    finalize_mutation_publication, prepare_mutation_publication,
};
mod active_changes;
mod authoring_metadata;
mod capture_settlement;
mod command_output;
mod conflict_inspection;
mod diff_render;
mod discovery;
mod edit_move;
mod frontier_service;
mod import_trace;
mod inspection;
mod label_service;
mod local;
mod log_inspection;
mod materialization;
mod remote;
mod remote_config;
mod repack;
mod repository_diff;
mod rewrite_transaction;
mod selection_recovery;
mod span_trace;
mod squash;
mod squash_attribution;
mod store_analyze;
mod store_lookup;
mod workspace;
mod workspace_clone;
mod workspace_diff;
mod workspace_files;
mod workspace_lifecycle;
mod workspace_metadata;
mod workspace_registry;
mod workspace_registry_service;
mod workspace_scan;
mod workspace_status;
mod workspace_walk;
use command_output::{
    CliFailure, OutputFormat, Success, clap_error, emit_error, emit_success, requested_format,
    require_supported_platform, write_stderr,
};
use discovery::{allocate_store_root, canonical_store_path, discover_workspace, store_home};
use local::{content_store, initialize, open, projection_path, revision_with_edits, runtime};
use workspace_metadata::{PendingRepositoryOperation, StoreMetadata, WorkspaceMetadata};

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
    /// Materialize the active label and its selected descendant frontier.
    Materialize,
    /// Switch to a label and one exact descendant frontier.
    Switch(FrontierSelectionArgs),
    /// Refresh the active label binding and descendant-frontier selection.
    Refresh(RefreshArgs),
    /// Inspect descendant frontiers of a native label.
    #[command(subcommand)]
    Frontier(FrontierCommand),
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
    /// List native short labels in deterministic name order.
    Labels,
    /// Create, rename, or delete native short labels.
    #[command(subcommand)]
    Label(LabelCommand),
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
    /// Squash every unpublished change in the selected stack.
    #[command(subcommand)]
    Stack(StackCommand),
    /// Inspect or detach workspaces sharing one repository store.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Storage analysis and maintenance.
    #[command(subcommand)]
    Store(StoreCommand),
    /// Repack a packed store without changing repository authority.
    Repack(RepackArgs),
    /// Import a local Git repository into a fresh Wheat repository.
    Import(ImportArgs),
    /// Project Wheat revision history and push it to a Git branch.
    Push(PushArgs),
    /// Clone a repository from a wheat-server via the native protocol.
    Clone(NativeCloneArgs),
    /// Fetch updates from a wheat-server via the native protocol.
    Fetch(NativeFetchArgs),
    /// Publish local state to a wheat-server via the native protocol.
    Publish(NativePublishArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Workspace to initialize. Defaults to the current directory.
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct FrontierSelectionArgs {
    label: String,
    /// Exact descendant frontier view. Required when more than one exists.
    #[arg(long)]
    frontier: Option<String>,
}

#[derive(Debug, Args)]
struct RefreshArgs {
    /// Exact descendant frontier view. Required when more than one exists.
    #[arg(long)]
    frontier: Option<String>,
}

#[derive(Debug, Subcommand)]
enum FrontierCommand {
    /// List bounded descendant frontier views.
    List {
        /// Label to inspect; defaults to the workspace's active label.
        #[arg(long)]
        label: Option<String>,
    },
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
    /// Show only changes at or below these repository-relative paths.
    paths: Vec<String>,
}

/// The filter for `wheat diff PATH…`: each path is repository-relative, and one
/// trailing slash (as shell completion adds to directories) is accepted.
fn diff_path_filter(paths: Vec<String>) -> Result<wheat_store::TreePathFilter> {
    let mut roots = Vec::with_capacity(paths.len());
    for path in paths {
        let root = path.strip_suffix('/').unwrap_or(&path);
        materialization::validate_path(root)
            .map_err(|error| CliFailure::invalid(format!("invalid diff path {path:?}: {error}")))?;
        roots.push(root.to_owned());
    }
    Ok(wheat_store::TreePathFilter::new(roots))
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// List workspaces registered against this repository store.
    List,
    /// Detach this workspace while preserving every materialized file.
    Unregister,
}

#[derive(Debug, Subcommand)]
enum StoreCommand {
    /// Show a detailed breakdown of where bytes live in the store.
    Analyze,
}

#[derive(Debug, Args)]
struct RepackArgs {
    /// Bypass the temporary-capacity preflight.
    #[arg(long)]
    force: bool,
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
    /// Collapse the unpublished checkpoint suffix of one logical change.
    Squash {
        change: Option<String>,
        #[command(flatten)]
        options: SquashOptions,
    },
}

#[derive(Debug, Subcommand)]
enum StackCommand {
    /// Collapse unpublished checkpoint suffixes throughout the selected stack.
    Squash(SquashOptions),
}

#[derive(Debug, clap::Args)]
struct SquashOptions {
    /// Do not add `Co-authored-by` trailers for checkpoint authors other than you, for
    /// example when they are you under an old email. Trailers already in the messages stay.
    #[arg(long)]
    no_coauthors: bool,
}

impl SquashOptions {
    fn coauthors(&self) -> squash_attribution::Coauthors {
        if self.no_coauthors {
            squash_attribution::Coauthors::Omit
        } else {
            squash_attribution::Coauthors::Credit
        }
    }
}

#[derive(Debug, Subcommand)]
enum LabelCommand {
    /// Bind a new short label to an immutable retained view.
    Create {
        name: String,
        /// Exact retained view; defaults to the workspace's selected view.
        #[arg(long)]
        view: Option<String>,
        /// Full repository head CAS guard: <generation>:<envelope>.
        #[arg(long, value_parser = label_service::parse_expected_head)]
        expected_head: Option<wheat_store::RepositoryHead>,
    },
    /// Rename a label without changing its view or import provenance.
    Rename {
        old: String,
        new: String,
        /// Full repository head CAS guard: <generation>:<envelope>.
        #[arg(long, value_parser = label_service::parse_expected_head)]
        expected_head: Option<wheat_store::RepositoryHead>,
    },
    /// Delete a label, unless it is selected by the current workspace.
    Delete {
        name: String,
        /// Full repository head CAS guard: <generation>:<envelope>.
        #[arg(long, value_parser = label_service::parse_expected_head)]
        expected_head: Option<wheat_store::RepositoryHead>,
    },
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
    /// Capture all non-ignored modified and deleted tracked paths instead of
    /// naming them explicitly. New untracked files are excluded; use explicit
    /// path arguments to include them.
    #[arg(long)]
    all: bool,
    /// Explicit repository-relative workspace paths. Required unless --all is used.
    paths: Vec<String>,
    #[command(flatten)]
    metadata: RevisionMetadataArgs,
    /// Wider guard as PATH:START:END, using byte offsets in canonical-LF workspace content.
    #[arg(long = "guard", value_parser = parse_guard)]
    guards: Vec<GuardRange>,
    /// Move blocks and files that later changes wrote into the change without asking, when
    /// your edit overlaps them.
    #[arg(long)]
    force: bool,
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
    write_stderr(format_args!("Change message: "));
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
    /// Git ref to use as the workspace's active label.
    /// All local branches, tags, and remote-tracking branches are imported
    /// regardless; this only selects which one becomes the workspace default.
    #[arg(long, default_value = "refs/heads/main")]
    branch: String,
    /// Write bounded internal import diagnostics as JSON Lines.
    #[arg(long, value_name = "PATH")]
    trace: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum PushSource {
    Label(String),
    View(ViewId),
}

impl FromStr for PushSource {
    type Err = String;

    fn from_str(source: &str) -> std::result::Result<Self, Self::Err> {
        if let Some(name) = source.strip_prefix("label:") {
            if name.is_empty() {
                return Err("label selector requires a name".into());
            }
            return Ok(Self::Label(name.into()));
        }
        ViewId::from_str(source)
            .map(Self::View)
            .map_err(|_| "source must be label:<name> or an exact view ID".into())
    }
}

#[derive(Debug, Args)]
struct PushArgs {
    /// Git remote URL. It is passed to Git and never persisted by Wheat.
    remote: String,
    /// Full destination branch ref, for example refs/heads/main.
    #[arg(long)]
    to: String,
    /// Advanced source override: label:<name> or an exact retained view ID.
    #[arg(long)]
    source: Option<PushSource>,
    /// Replace an unknown/diverged destination only if it still has the observed OID.
    #[arg(long)]
    force_with_lease: bool,
}

#[derive(Debug, Args)]
struct NativeCloneArgs {
    /// Wheat server URL: wheat://host:port/owner/repo.
    url: String,
    /// Workspace directory. Defaults to a directory named after the repository.
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct NativeFetchArgs {
    /// Override the remote URL (wheat://host:port/owner/repo).
    #[arg(long)]
    remote: Option<String>,
    /// Force fetch even if local has diverged from last-fetched state.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct NativePublishArgs {
    /// Override the remote URL (wheat://host:port/owner/repo).
    #[arg(long)]
    remote: Option<String>,
    /// Force publish (skip monotonicity checks on the server).
    #[arg(long)]
    force: bool,
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
    // Reports when dropped, after the command's output.
    let _span_trace = span_trace::install_from_env();
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
    match tracing::info_span!("command", command).in_scope(|| execute(cli)) {
        Ok(success) => emit_success(format, &success),
        Err(error) => emit_error(format, command, &error),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init(_) => "init",
        Command::Materialize => "materialize",
        Command::Switch(_) => "switch",
        Command::Refresh(_) => "refresh",
        Command::Frontier(FrontierCommand::List { .. }) => "frontier.list",
        Command::Restore(_) => "restore",
        Command::Status => "status",
        Command::Validate => "validate",
        Command::Log => "log",
        Command::Changes => "changes",
        Command::Labels => "labels",
        Command::Label(LabelCommand::Create { .. }) => "label.create",
        Command::Label(LabelCommand::Rename { .. }) => "label.rename",
        Command::Label(LabelCommand::Delete { .. }) => "label.delete",
        Command::Conflicts => "conflicts",
        Command::Diff(_) => "diff",
        Command::Config(ConfigCommand::Set { .. }) => "config.set",
        Command::Edits { .. } => "edits",
        Command::Add(_) => "add",
        Command::Cat { .. } => "cat",
        Command::Change(ChangeCommand::New(_)) => "change.new",
        Command::Change(ChangeCommand::Select { .. }) => "change.select",
        Command::Change(ChangeCommand::Squash { .. }) => "change.squash",
        Command::Stack(StackCommand::Squash(_)) => "stack.squash",
        Command::Resolve(_) => "resolve",
        Command::Workspace(WorkspaceCommand::List) => "workspace.list",
        Command::Workspace(WorkspaceCommand::Unregister) => "workspace.unregister",
        Command::Store(StoreCommand::Analyze) => "store.analyze",
        Command::Repack(_) => "repack",
        Command::Import(_) => "import",
        Command::Push(_) => "push",
        Command::Clone(_) => "clone",
        Command::Fetch(_) => "fetch",
        Command::Publish(_) => "publish",
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
        Command::Materialize => {
            materialize_active_workspace(&requested_workspace, cli.store.as_deref(), "materialize")
        }
        Command::Switch(arguments) => switch_workspace(
            &requested_workspace,
            cli.store.as_deref(),
            arguments.label,
            parse_optional_view(arguments.frontier)?,
            "switch",
        ),
        Command::Refresh(arguments) => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            switch_workspace(
                &requested_workspace,
                cli.store.as_deref(),
                context.workspace.active_label,
                parse_optional_view(arguments.frontier)?,
                "refresh",
            )
        }
        Command::Frontier(FrontierCommand::List { label }) => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let label = label.unwrap_or(context.workspace.active_label);
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let document = runtime()?.block_on(frontier_service::list(&local, &label))?;
            let mut human = String::new();
            for frontier in &document.frontiers {
                writeln!(human, "{frontier}")?;
            }
            Success::new("frontier.list", &document, human)
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
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            let scan = workspace_scan::scan(
                &context.root,
                &local,
                &context.workspace,
                0,
                &wheat_store::TreePathFilter::default(),
            )?;

            // Discover untracked files via workspace walk with ignore rules.
            let global = wheat_ignore::loader::read_global_ignore();
            let global_ref = global
                .as_ref()
                .map(|(content, path)| (content.as_str(), path.clone()));
            let mut ignore_stack = wheat_ignore::stack::IgnoreStack::new(global_ref, &[]);
            let walk_result = workspace_walk::walk_workspace(
                &context.root,
                &scan.tracked_paths,
                &mut ignore_stack,
                workspace_walk::WalkLimits::default(),
            )?;

            let status = workspace_status::WorkspaceStatus::from_scan(scan, Some(walk_result));

            let mut output = String::new();
            // Print diagnostics to stderr (they're informational, not command output).
            for diagnostic in &status.diagnostics {
                write_stderr(format_args!("note: {diagnostic}\n"));
            }
            writeln!(output, "active label: {}", status.active_label)?;
            writeln!(output, "basis view: {}", status.basis)?;
            writeln!(output, "effective view: {}", status.effective_view)?;
            writeln!(output, "tracked changes: {}", status.changes.len())?;
            if status.label_binding_drifted {
                writeln!(output, "active label binding changed since refresh")?;
            }
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
            let untracked_count = status.untracked.iter().filter(|u| !u.ignored).count();
            if untracked_count > 0 {
                writeln!(output, "untracked files: {untracked_count}")?;
                for entry in &status.untracked {
                    if !entry.ignored {
                        let suffix = if entry.is_directory { "/" } else { "" };
                        writeln!(output, "untracked {}{suffix}", entry.path)?;
                    }
                }
            }
            Success::new("status", &status, output)
        }
        Command::Validate => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let local = open(&store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&store_root, &local)?;
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
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let workspace = WorkspaceMetadata::read(&context.root)?;
            // Packed repositories hold their catalog lock for the handle lifetime; admission is
            // retained independently while the inspector reopens the same store.
            drop(local);
            log_inspection::inspect(&context.store_root, workspace.effective_view(), format)
        }
        Command::Changes => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            runtime()?.block_on(materialization::recover_locked(
                &context.root,
                &local,
                &mut context.workspace,
            ))?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let basis = context.workspace.basis;
            let head = local.heads.read()?;
            let document = runtime()?.block_on(active_changes::inspect(
                &local,
                head,
                basis,
                context.workspace.effective_view(),
                context.workspace.selected_change,
            ))?;
            let human = active_changes::render_human(&document)?;
            Success::new("changes", &document, human)
        }
        Command::Labels => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let local = open(&store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&store_root, &local)?;
            label_service::list(&local)
        }
        Command::Label(arguments) => {
            let explicit_store = cli.store.as_deref().map(canonical_store_path).transpose()?;
            let context = if let Some(store) = &explicit_store {
                discover_workspace(&requested_workspace)
                    .ok()
                    .filter(|context| &context.store_root == store)
            } else {
                Some(resolve_context(&requested_workspace, None)?)
            };
            let store_root = explicit_store
                .or_else(|| context.as_ref().map(|context| context.store_root.clone()))
                .expect("workspace resolution is required without --store");
            let local = open(&store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&store_root, &local)?;
            match arguments {
                LabelCommand::Create {
                    name,
                    view,
                    expected_head,
                } => {
                    let view = match view {
                        Some(view) => ViewId::from_str(&view).context("invalid view ID")?,
                        None => context
                            .as_ref()
                            .map(|context| context.workspace.effective_view())
                            .ok_or_else(|| {
                                CliFailure::invalid(
                                    "label create without a workspace requires an explicit --view",
                                )
                            })?,
                    };
                    label_service::create(&local, name, view, expected_head)
                }
                LabelCommand::Rename {
                    old,
                    new,
                    expected_head,
                } => {
                    reject_selected_label_mutation(context.as_ref(), &old, "rename")?;
                    label_service::rename(&local, old, new, expected_head)
                }
                LabelCommand::Delete {
                    name,
                    expected_head,
                } => {
                    reject_selected_label_mutation(context.as_ref(), &name, "delete")?;
                    label_service::delete(&local, name, expected_head)
                }
            }
        }
        Command::Conflicts => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let workspace = WorkspaceMetadata::read(&context.root)?;
            let document = runtime()?.block_on(conflict_inspection::inspect(
                &local,
                workspace.effective_view(),
            ))?;
            let mut human = Vec::new();
            conflict_inspection::human(&document, &mut human)?;
            Success::new("conflicts", &document, human)
        }
        Command::Diff(arguments) => {
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            let filter = diff_path_filter(arguments.paths)?;
            let limits = wheat_core::diff_model::DiffLimits::default();
            let deadline = std::time::Instant::now() + limits.deadline;
            let document = if let Some(revision) = arguments.revision {
                let revision = RevisionId::from_str(&revision)
                    .map_err(|_| CliFailure::invalid("invalid revision ID"))?;
                let input = runtime()?.block_on(repository_diff::revision(
                    &local, revision, &filter, deadline,
                ))?;
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
                let input = runtime()?.block_on(repository_diff::views(
                    &local, &from, &to, &filter, deadline,
                ))?;
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
                    &filter,
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
                authoring_metadata::ConfigKey::CoreEditor => "core.editor",
                authoring_metadata::ConfigKey::CorePager => "core.pager",
                authoring_metadata::ConfigKey::CoreDefaultLabel => "core.default_label",
                authoring_metadata::ConfigKey::CoreDefaultFormat => "core.default_format",
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
            if arguments.all && !arguments.paths.is_empty() {
                return Err(CliFailure::invalid(
                    "--all and explicit paths are mutually exclusive",
                ));
            }
            if !arguments.all && arguments.paths.is_empty() {
                return Err(CliFailure::invalid(
                    "at least one workspace path is required; use --all to capture all modified tracked paths",
                ));
            }
            let mut context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            let local = open(&context.store_root)?;
            // Store admission precedes the workspace lock. Holding both across planning and
            // publication prevents either a staged rewrite or another head mutation from
            // interleaving before the compare-and-swap.
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            let runtime = runtime()?;
            runtime.block_on(materialization::recover(
                &context.root,
                &local,
                &mut context.workspace,
            ))?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;

            // Resolve paths: --all discovers modified/deleted tracked paths via scan.
            let paths = if arguments.all {
                let scan = workspace_scan::scan_locked(
                    &context.root,
                    &local,
                    &context.workspace,
                    0,
                    &wheat_store::TreePathFilter::default(),
                )?;
                let resolved: Vec<String> = scan.changes.iter().map(|c| c.path.clone()).collect();
                if resolved.is_empty() {
                    return Err(CliFailure::invalid(
                        "no modified or deleted tracked paths; workspace is clean",
                    ));
                }
                resolved
            } else {
                arguments.paths
            };

            let expected = local.heads.read()?;
            let basis = context.workspace.basis;
            let active = runtime.block_on(active_changes::inspect(
                &local,
                expected,
                basis,
                context.workspace.effective_view(),
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
            let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads);
            let session = runtime.block_on(mutator.open_authoring(
                expected,
                context.workspace.basis,
                context.workspace.effective_view(),
            ))?;
            let authored = runtime.block_on(workspace::author_from_workspace(
                &workspace::AddTarget {
                    root: &context.root,
                    objects: &objects,
                    store: &local.content,
                    mutator: &mutator,
                    session: &session,
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
                &guards,
                revision_metadata,
            ))?;
            if !authored.moves.is_empty() && !arguments.force {
                let stdin = std::io::stdin();
                let stderr = std::io::stderr();
                let interactive =
                    format == OutputFormat::Human && stdin.is_terminal() && stderr.is_terminal();
                edit_move::confirm(
                    change,
                    &authored.moves,
                    &mut edit_move::PromptIo {
                        input: &mut stdin.lock(),
                        diagnostics: &mut stderr.lock(),
                        interactive,
                    },
                )?;
            }
            let (prepared, outcome) =
                runtime.block_on(mutator.revise_changes_in(session, authored.requests))?;
            let revision_id = outcome.revisions[&change];
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
            workspace_registry_service::reconcile(
                &context.root,
                &context.store_root,
                &context.workspace,
            )?;
            let path_count = paths.len();
            let mut output = String::new();
            writeln!(
                output,
                "added {path_count} path(s) to change {change} — {title}",
            )?;
            writeln!(output, "revision {revision_id}")?;
            for line in &authored.moves {
                writeln!(output, "moved {line}")?;
            }
            if arguments.change.is_some()
                && context.workspace.selected_change != Some(change)
                && let Some(selected) = context.workspace.selected_change
            {
                writeln!(output, "selected change remains {selected}")?;
            }
            print_reapplied(&mut output, &outcome.reapplied)?;
            let materialize_required = settlement
                .as_ref()
                .is_none_or(|settlement| !settlement.complete);
            let diverged = settlement
                .as_ref()
                .map_or(&[][..], |settlement| settlement.diverged.as_slice());
            for path in diverged {
                writeln!(output, "diverged {path}")?;
            }
            if materialize_required {
                writeln!(
                    output,
                    "lineage changed: the working view includes changes this workspace has not materialized; run `wheat materialize` to update it",
                )?;
            }
            Success::new(
                "add",
                json!({
                    "change": change.to_string(),
                    "revision": revision_id.to_string(),
                    "paths": path_count,
                    "moved": authored.moved.iter().map(|moved| json!({
                        "path": moved.path,
                        "from": moved.from.to_string(),
                        "revision": outcome.revisions[&moved.from].to_string(),
                        "edit": moved.edit.to_string(),
                    })).collect::<Vec<_>>(),
                    "reapplied": outcome.reapplied.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "materialize_required": materialize_required,
                    "diverged": diverged,
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
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
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
            let expected_given = edit.expected.is_some();
            let edits = if has_edit {
                capture_initial_edit(&local.content, edit)?
            } else {
                RevisionEdits::default()
            };
            let change = ChangeId::new();
            let proposal = ChangeProposal {
                id: change,
                metadata: change_metadata.expect("new change metadata requested"),
                dependencies,
                revision_metadata,
                edits,
            };
            let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
            let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads);
            let basis = context.workspace.basis;
            let effective = context.workspace.effective_view();
            let created = if use_frontier {
                runtime.block_on(mutator.create_on_frontier(expected, basis, effective, proposal))
            } else {
                runtime.block_on(mutator.create(expected, basis, effective, proposal))
            };
            let (prepared, outcome) =
                created.map_err(|error| explain_initial_edit(error.into(), expected_given))?;
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
            });
            workspace_registry_service::reconcile(
                &context.root,
                &context.store_root,
                &context.workspace,
            )?;
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
            workspace_registry_service::reconcile(
                &context.root,
                &context.store_root,
                &context.workspace,
            )?;
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
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
            let _workspace_lock = materialization::WorkspaceLock::acquire(&context.root)?;
            context.workspace = WorkspaceMetadata::read(&context.root)?;
            runtime()?.block_on(materialization::recover_locked(
                &context.root,
                &local,
                &mut context.workspace,
            ))?;
            selection_recovery::recover(&context.root, &local, &mut context.workspace)?;
            let basis = context.workspace.basis;
            let head = local.heads.read()?;
            let active = runtime()?.block_on(active_changes::inspect(
                &local,
                head,
                basis,
                context.workspace.effective_view(),
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
            workspace_registry_service::reconcile(
                &context.root,
                &context.store_root,
                &context.workspace,
            )?;
            Success::new(
                "change.select",
                json!({ "change": change.to_string(), "title": selected.title }),
                format!("selected change {change} — {}\n", selected.title),
            )
        }
        Command::Change(ChangeCommand::Squash { change, options }) => squash::execute(
            &requested_workspace,
            cli.store.as_deref(),
            squash::Scope::Change(change.as_deref()),
            options.coauthors(),
        ),
        Command::Stack(StackCommand::Squash(options)) => squash::execute(
            &requested_workspace,
            cli.store.as_deref(),
            squash::Scope::Stack,
            options.coauthors(),
        ),
        Command::Resolve(arguments) => {
            let context = resolve_context(&requested_workspace, cli.store.as_deref())?;
            resolve(&context.root, &context.store_root, arguments)
        }
        Command::Workspace(WorkspaceCommand::List) => {
            list_workspaces(&requested_workspace, cli.store.as_deref())
        }
        Command::Workspace(WorkspaceCommand::Unregister) => {
            unregister_workspace(&requested_workspace, cli.store.as_deref())
        }
        Command::Store(StoreCommand::Analyze) => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            store_analyze::analyze(&store_root)
        }
        Command::Repack(arguments) => {
            let store_root = resolve_store_root(&requested_workspace, cli.store.as_deref())?;
            let local = open(&store_root)?;
            let _operation =
                rewrite_transaction::StoreOperationGuard::acquire(&store_root, &local)?;
            let backend = repack::PackedRepackBackend::new(&store_root, &local);
            let stdin = std::io::stdin();
            let stderr = std::io::stderr();
            let interactive = stdin.is_terminal() && stderr.is_terminal();
            let mut input = stdin.lock();
            let mut diagnostics = stderr.lock();
            let mut io = repack::RepackIo {
                input: &mut input,
                diagnostics: &mut diagnostics,
                interactive,
            };
            repack::run(
                &backend,
                arguments.force,
                || repack::available_bytes(&store_root),
                &mut io,
            )
        }
        Command::Import(arguments) => import(arguments, cli.store.as_deref(), &requested_workspace),
        Command::Clone(arguments) => native_clone(arguments, cli.store.as_deref()),
        Command::Fetch(arguments) => {
            native_fetch(&requested_workspace, cli.store.as_deref(), arguments)
        }
        Command::Publish(arguments) => {
            native_publish(&requested_workspace, cli.store.as_deref(), arguments)
        }
        Command::Push(mut arguments) => {
            let store_root = if arguments.source.is_none() {
                let context = resolve_context(&requested_workspace, cli.store.as_deref()).map_err(
                    |_| {
                        CliFailure::invalid(
                            "push without --source requires a discoverable matching Wheat workspace",
                        )
                    },
                )?;
                arguments.source = Some(PushSource::View(context.workspace.effective_view()));
                context.store_root
            } else {
                resolve_store_root(&requested_workspace, cli.store.as_deref())?
            };
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
    let mut trace = arguments
        .trace
        .as_deref()
        .map(import_trace::ImportTrace::create)
        .transpose()?;
    let local_source = Path::new(&arguments.source);
    let clone;
    let remote_clone = !local_source.exists();
    if let Some(trace) = &mut trace {
        trace.started(remote_clone);
    }
    let source = if !remote_clone {
        local_source
    } else {
        let started = std::time::Instant::now();
        if let Some(trace) = &mut trace {
            trace.phase_started("remote_clone");
        }
        clone = clone_remote_for_import(&arguments.source)?;
        if let Some(trace) = &mut trace {
            trace.phase_finished("remote_clone", started.elapsed());
        }
        clone.path()
    };
    let runtime = runtime()?;
    let head_result = if let Some(trace) = &mut trace {
        runtime.block_on(wheat_git::atomic_import_with_observer(
            source,
            &store_root,
            &arguments.branch,
            |event| trace.atomic_event(event),
        ))
    } else {
        runtime.block_on(wheat_git::atomic_import(
            source,
            &store_root,
            &arguments.branch,
        ))
    };
    let head = match head_result {
        Ok(head) => head,
        Err(error) => {
            if let Some(mut trace) = trace {
                trace.error("atomic_import");
                let _ = trace.finish();
            }
            return Err(error.into());
        }
    };
    let phase_started = std::time::Instant::now();
    if let Some(trace) = &mut trace {
        trace.phase_started("store_metadata");
    }
    let store = StoreMetadata::create(&store_root)?;
    let canonical_store = canonical_store_path(&store_root)?;
    if let Some(trace) = &mut trace {
        trace.phase_finished("store_metadata", phase_started.elapsed());
    }
    let selected_label = wheat_git::ref_to_label_name(&arguments.branch).to_owned();
    let phase_started = std::time::Instant::now();
    if let Some(trace) = &mut trace {
        trace.phase_started("resolve_frontier");
    }
    let repository = open(&store_root)?;
    let resolved = runtime.block_on(frontier_service::resolve_freshly_validated_import(
        &repository,
        head,
        &selected_label,
    ))?;
    if let Some(trace) = &mut trace {
        trace.phase_finished("resolve_frontier", phase_started.elapsed());
    }
    let workspace = WorkspaceMetadata::new(
        canonical_store,
        store.store_id,
        selected_label.clone(),
        resolved.basis,
    );
    drop(repository);
    let phase_started = std::time::Instant::now();
    if let Some(trace) = &mut trace {
        trace.phase_started("workspace_metadata");
    }
    initialize_workspace_directory(workspace_root)?;
    workspace_registry_service::reconcile(workspace_root, &store_root, &workspace)?;
    workspace.initialize(workspace_root)?;
    if let Some(trace) = &mut trace {
        trace.phase_finished("workspace_metadata", phase_started.elapsed());
    }
    let phase_started = std::time::Instant::now();
    if let Some(trace) = &mut trace {
        trace.phase_started("materialize");
    }
    let (view, materialized_root) =
        materialize_resolved(workspace_root, Some(&store_root), selected_label, resolved)?;
    if let Some(trace) = &mut trace {
        trace.phase_finished("materialize", phase_started.elapsed());
    }
    if let Some(mut trace) = trace {
        trace.complete(head.generation);
        trace.finish()?;
    }
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
    let repository = open(&store_root)?;
    let resolved = runtime()?.block_on(frontier_service::resolve(&repository, "main", None))?;
    drop(repository);
    initialize_workspace_directory(workspace_root)?;
    let workspace = WorkspaceMetadata::new(
        canonical_store,
        store.store_id,
        "main".into(),
        resolved.basis,
    );
    workspace_registry_service::reconcile(workspace_root, &store_root, &workspace)?;
    workspace.initialize(workspace_root)?;
    let (view, materialized_root) =
        materialize_resolved(workspace_root, Some(&store_root), "main".into(), resolved)?;
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

fn reject_selected_label_mutation(
    context: Option<&discovery::ResolvedWorkspace>,
    name: &str,
    operation: &str,
) -> Result<()> {
    if context.is_some_and(|context| context.workspace.active_label == name) {
        return Err(CliFailure::invalid(format!(
            "cannot {operation} label {name}: it is selected by this workspace"
        )));
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

fn materialize_active_workspace(
    workspace: &Path,
    explicit_store: Option<&Path>,
    command: &'static str,
) -> Result<Success> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &repository)?;
    let _lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    runtime()?.block_on(materialization::recover_locked(
        &context.root,
        &repository,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &repository, &mut context.workspace)?;
    let resolved = runtime()?.block_on(frontier_service::resolve(
        &repository,
        &context.workspace.active_label,
        context.workspace.selected_frontier,
    ))?;
    let view = resolved.effective_view();
    let label = context.workspace.active_label.clone();
    runtime()?.block_on(materialization::materialize_locked(
        &context.root,
        &repository,
        &mut context.workspace,
        resolved.head,
        label,
        resolved.basis,
        resolved.selected_frontier,
        resolved.tree,
    ))?;
    Success::new(
        command,
        json!({ "view": view.to_string() }),
        format!("materialized view {view} in {}\n", context.root.display()),
    )
}

fn switch_workspace(
    workspace: &Path,
    explicit_store: Option<&Path>,
    label: String,
    requested_frontier: Option<ViewId>,
    command: &'static str,
) -> Result<Success> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &repository)?;
    let _lock = materialization::WorkspaceLock::acquire(&context.root)?;
    context.workspace = WorkspaceMetadata::read(&context.root)?;
    runtime()?.block_on(materialization::recover_locked(
        &context.root,
        &repository,
        &mut context.workspace,
    ))?;
    selection_recovery::recover(&context.root, &repository, &mut context.workspace)?;
    ensure_workspace_clean_locked(&context.root, &repository, &context.workspace)?;
    let resolved = runtime()?.block_on(frontier_service::resolve(
        &repository,
        &label,
        requested_frontier,
    ))?;
    let view = resolved.effective_view();
    runtime()?.block_on(materialization::materialize_locked(
        &context.root,
        &repository,
        &mut context.workspace,
        resolved.head,
        label.clone(),
        resolved.basis,
        resolved.selected_frontier,
        resolved.tree,
    ))?;
    let root = context.root;
    Success::new(
        command,
        json!({ "label": label, "view": view.to_string() }),
        format!(
            "{command} selected label {label} at view {view} in {}\n",
            root.display()
        ),
    )
}

fn materialize_resolved(
    workspace: &Path,
    explicit_store: Option<&Path>,
    label: String,
    resolved: frontier_service::ResolvedFrontier,
) -> Result<(ViewId, PathBuf)> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let view = resolved.effective_view();
    runtime()?.block_on(materialization::materialize(
        &context.root,
        &repository,
        &mut context.workspace,
        resolved.head,
        label,
        resolved.basis,
        resolved.selected_frontier,
        resolved.tree,
    ))?;
    Ok((view, context.root))
}

fn ensure_workspace_clean_locked(
    root: &Path,
    repository: &local::LocalRepository,
    workspace: &WorkspaceMetadata,
) -> Result<()> {
    let scan = workspace_scan::scan_locked(
        root,
        repository,
        workspace,
        0,
        &wheat_store::TreePathFilter::default(),
    )?;
    if !scan.changes.is_empty() {
        return Err(CliFailure::invalid(
            "uncaptured tracked workspace edits block switch or refresh",
        ));
    }
    Ok(())
}

fn parse_optional_view(value: Option<String>) -> Result<Option<ViewId>> {
    value
        .map(|value| ViewId::from_str(&value).context("invalid frontier view ID"))
        .transpose()
}

fn restore_workspace(
    workspace: &Path,
    explicit_store: Option<&Path>,
    paths: &[String],
) -> Result<usize> {
    let mut context = resolve_context(workspace, explicit_store)?;
    let repository = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &repository)?;
    let restored = runtime()?.block_on(materialization::restore(
        &context.root,
        &repository,
        &mut context.workspace,
        paths,
    ))?;
    Ok(restored)
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
    let _operation = rewrite_transaction::StoreOperationGuard::acquire(root, &local)?;
    let expected = local.heads.read()?;
    let runtime = runtime()?;
    let (base, selected, changes) = runtime.block_on(async {
        let reader = RepositoryReader::new(&local.objects, &local.content);
        let envelope = reader.validate_envelope(expected.envelope).await?;
        let state = reader.state(envelope.state).await?;
        let selected = match arguments
            .source
            .as_ref()
            .context("push source was not resolved")?
        {
            PushSource::Label(name) => reader
                .view_label_manifest(envelope.labels)
                .await?
                .labels
                .get(name)
                .map(|label| label.view)
                .with_context(|| format!("unknown Wheat label {name}"))?,
            PushSource::View(view) => *view,
        };
        Ok::<_, anyhow::Error>((state.base_view, selected, state.changes))
    })?;
    let prepared = runtime.block_on(wheat_git::bridge::prepare_push(
        &local.objects,
        &local.content,
        selected,
        base,
        changes,
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
            "pushed {projected} commit{} (one per change) to {} at {tip}\n",
            if projected == 1 { "" } else { "s" },
            arguments.to
        )
    };
    Success::new(
        "push",
        json!({
            "destination": arguments.to,
            "tip": tip.to_string(),
            "projected_commits": projected,
            "up_to_date": up_to_date,
        }),
        human,
    )
}

fn resolve(
    workspace_root: &std::path::Path,
    store_root: &std::path::Path,
    arguments: ResolveArgs,
) -> Result<Success> {
    if arguments.path.len() > conflict_inspection::MAX_PATH_BYTES {
        return Err(CliFailure::limit(
            "resolution path exceeds conflict inspection limit",
        ));
    }
    let local = open(store_root)?;
    let _operation = rewrite_transaction::StoreOperationGuard::acquire(store_root, &local)?;
    let _workspace_lock = materialization::WorkspaceLock::acquire(workspace_root)?;
    let mut metadata = WorkspaceMetadata::read(workspace_root)?;
    let runtime = runtime()?;
    let revision = if let Some(revision) = arguments.revision.as_deref() {
        RevisionId::from_str(revision).map_err(|_| CliFailure::invalid("invalid revision ID"))?
    } else {
        let document = runtime.block_on(conflict_inspection::inspect(
            &local,
            metadata.effective_view(),
        ))?;
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
        let working = reader.view(metadata.effective_view()).await?;
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
    let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads);
    let (prepared, outcome) = runtime.block_on(mutator.resolve_conflict(
        expected,
        metadata.basis,
        metadata.effective_view(),
        resolution,
    ))?;
    let resulting_frontier = prepared.resulting_view();
    let publication = runtime.block_on(prepare_mutation_publication(
        &objects,
        &local.content,
        &local.heads,
        expected,
        prepared,
    ))?;
    metadata.pending_repository_operation = Some(PendingRepositoryOperation {
        expected_head: publication.expected().into(),
        proposed_head: publication.proposed().into(),
        resulting_frontier,
        selected_change: metadata.selected_change,
    });
    workspace_registry_service::reconcile(workspace_root, store_root, &metadata)?;
    metadata
        .replace_prepared(workspace_root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    runtime.block_on(objects.flush())?;
    finalize_mutation_publication(&local.heads, publication)?;
    metadata.selected_frontier =
        (resulting_frontier != metadata.basis).then_some(resulting_frontier);
    metadata.pending_repository_operation = None;
    metadata
        .replace_prepared(workspace_root)?
        .publish()
        .map_err(|error| anyhow::anyhow!(error))?;
    workspace_registry_service::reconcile(workspace_root, store_root, &metadata)?;
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

/// Adds how to fix a `change new` initial edit whose `--expected` does not match the
/// path, which is the usual way it fails to apply. A whole-file edit fails with
/// `ExpectedFileMismatch`; a text edit against a missing or different file fails with
/// `KindMismatch` or `NoMatch`.
fn explain_initial_edit(error: anyhow::Error, expected_given: bool) -> anyhow::Error {
    let Some(RepositoryError::InvalidEdits(rejected)) = command_output::repository_error(&error)
    else {
        return error;
    };
    let hint = rejected.iter().find_map(|edit| {
        let path = &edit.path;
        match (&edit.reason, edit.target.is_some(), expected_given) {
            (EditFailure::ExpectedFileMismatch, true, false) => Some(format!(
                "{path} already exists; pass --expected with its current content to change it"
            )),
            (EditFailure::ExpectedFileMismatch | EditFailure::KindMismatch, false, true) => Some(
                format!("{path} does not exist; omit --expected to create it"),
            ),
            (
                EditFailure::ExpectedFileMismatch
                | EditFailure::KindMismatch
                | EditFailure::NoMatch,
                true,
                true,
            ) => Some(format!(
                "{path} does not match --expected; check its current content and --expected-kind"
            )),
            _ => None,
        }
    });
    match hint {
        Some(hint) => error.context(hint),
        None => error,
    }
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

// ---------------------------------------------------------------------------
// Native protocol commands: clone, fetch, publish
// ---------------------------------------------------------------------------

fn native_clone(arguments: NativeCloneArgs, explicit_store: Option<&Path>) -> Result<Success> {
    if let Some(source) = workspace_clone::local_source(&arguments.url) {
        let workspace_root = arguments.workspace.unwrap_or_else(|| {
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            PathBuf::from(format!("{name}-workspace"))
        });
        if let Some(store) = explicit_store {
            let source_store = discover_workspace(&source)
                .map(|context| context.store_root)
                .unwrap_or_else(|_| source.clone());
            if canonical_store_path(store)? != canonical_store_path(&source_store)? {
                return Err(CliFailure::invalid(
                    "explicit Wheat store does not match the local clone source",
                ));
            }
        }
        return workspace_clone::clone_local(&source, &workspace_root);
    }
    let parsed = remote::parse_wheat_url(&arguments.url)?;

    // Derive workspace directory from repo name if not provided.
    let workspace_root = arguments.workspace.unwrap_or_else(|| {
        let name = parsed
            .repo_name
            .rsplit('/')
            .next()
            .unwrap_or(&parsed.repo_name);
        PathBuf::from(name)
    });

    ensure_fresh_workspace(&workspace_root)?;

    let existing_store = if let Some(store) = explicit_store {
        let store = canonical_store_path(store)?;
        let config = remote_config::RemoteConfig::read(&store)?
            .context("explicit Wheat store has no native remote configuration")?;
        if !config.has_same_identity(&parsed) {
            return Err(CliFailure::invalid(
                "explicit Wheat store tracks a different native remote",
            ));
        }
        Some(store)
    } else {
        store_lookup::find_remote_store(&store_home()?, &parsed)?
    };
    if let Some(store_root) = existing_store {
        return workspace_clone::clone_local(&store_root, &workspace_root);
    }

    let store_root = allocate_store_root()?;
    if store_root.exists() {
        return Err(CliFailure::invalid(format!(
            "Wheat store destination already exists: {}",
            store_root.display()
        )));
    }
    std::fs::create_dir(&store_root)?;

    let objects = wheat_store::PackedObjectStore::open(store_root.join("storage"))?;
    let content = objects.content_store();
    let heads = wheat_store::LocalPublicationStore::new(store_root.join("repository.head"));

    let rt = runtime()?;
    let (remote_state, label_name) = rt.block_on(async {
        let mut session = remote::connect(&parsed.ws_url).await?;
        let state = remote::open_repository(&mut session, &parsed.repo_name).await?;

        // Fetch all structural objects.
        let obj_count = remote::fetch_objects(
            &mut session,
            1,
            &objects,
            &content,
            state.envelope,
            0,
            &[0u8; 32],
        )
        .await?;

        // Fetch all content.
        let content_count =
            remote::fetch_content(&mut session, 2, &objects, &content, state.envelope).await?;

        write_stderr(format_args!(
            "fetched {obj_count} objects, {content_count} content blobs\n"
        ));

        remote::close(&mut session).await?;
        // Pick the first label as the active label, or "main" as default.
        let label = state
            .labels
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "main".to_string());
        Ok::<_, anyhow::Error>((state, label))
    })?;
    // Release the packed store's process lock; the workspace reopens the store below.
    drop(content);
    drop(objects);

    // Write the head.
    let head = wheat_store::RepositoryHead {
        generation: remote_state.generation,
        envelope: remote_state.envelope,
    };
    heads.initialize(head)?;

    // Write store metadata.
    let store = workspace_metadata::StoreMetadata::create(&store_root)?;
    let canonical_store = canonical_store_path(&store_root)?;

    // Write remote config.
    let remote_cfg = remote_config::RemoteConfig::new(&arguments.url, &head);
    remote_cfg.write(&store_root)?;

    // Initialize workspace and materialize. If multiple descendant
    // frontiers exist, resolve at the label basis (the user can select
    // a specific frontier later with `wheat switch --frontier`).
    initialize_workspace_directory(&workspace_root)?;
    let repository = open(&store_root)?;
    let resolved = match rt.block_on(frontier_service::resolve(&repository, &label_name, None)) {
        Ok(r) => r,
        Err(_) => {
            // Multiple frontiers — resolve at the label's basis view.
            let basis_view = rt.block_on(async {
                let reader =
                    wheat_store::RepositoryReader::new(&repository.objects, &repository.content);
                let head = repository.heads.read()?;
                let envelope = reader.envelope(head.envelope).await?;
                let labels = reader.view_label_manifest(envelope.labels).await?;
                let label_entry = labels
                    .labels
                    .get(&label_name)
                    .ok_or_else(|| anyhow::anyhow!("unknown label {label_name}"))?;
                Ok::<_, anyhow::Error>(label_entry.view)
            })?;
            rt.block_on(frontier_service::resolve(
                &repository,
                &label_name,
                Some(basis_view),
            ))?
        }
    };
    drop(repository);
    let workspace = workspace_metadata::WorkspaceMetadata::new(
        canonical_store,
        store.store_id,
        label_name.clone(),
        resolved.basis,
    );
    workspace_registry_service::reconcile(&workspace_root, &store_root, &workspace)?;
    workspace.initialize(&workspace_root)?;
    let (view, materialized_root) =
        materialize_resolved(&workspace_root, Some(&store_root), label_name, resolved)?;

    let human = format!(
        "cloned {} into {}\nmaterialized view {view}\ngeneration {}, envelope {}\n",
        arguments.url,
        materialized_root.display(),
        head.generation,
        head.envelope,
    );
    Success::new(
        "clone",
        json!({
            "url": arguments.url,
            "generation": head.generation,
            "envelope": head.envelope.to_string(),
        }),
        human,
    )
}

fn native_fetch(
    workspace: &Path,
    explicit_store: Option<&Path>,
    arguments: NativeFetchArgs,
) -> Result<Success> {
    let context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
    let local_head = local.heads.read()?;

    // Resolve remote URL.
    let (parsed, remote_cfg) = resolve_remote(&context.store_root, arguments.remote.as_deref())?;

    let last_fetched = remote_cfg.last_fetched_head()?;

    // Divergence check: if local head has advanced past last-fetched, the user
    // has unpublished local work. Refuse unless --force.
    if !arguments.force && local_head.generation > last_fetched.generation {
        return Err(CliFailure::stale(
            "local repository has unpublished changes; run `wheat publish` first, or use `wheat fetch --force` to discard local divergence",
        ));
    }

    let rt = runtime()?;
    let (remote_state, obj_count, content_count) = rt.block_on(async {
        let mut session = remote::connect(&parsed.ws_url).await?;
        let state = remote::open_repository(&mut session, &parsed.repo_name).await?;

        if state.generation == last_fetched.generation && state.envelope == last_fetched.envelope {
            remote::close(&mut session).await?;
            return Ok((state, 0, 0));
        }

        let obj_count = remote::fetch_objects(
            &mut session,
            1,
            &local.objects,
            &local.content,
            state.envelope,
            0,
            &[0u8; 32],
        )
        .await?;

        let content_count = remote::fetch_content(
            &mut session,
            2,
            &local.objects,
            &local.content,
            state.envelope,
        )
        .await?;

        remote::close(&mut session).await?;
        Ok::<_, anyhow::Error>((state, obj_count, content_count))
    })?;

    if obj_count == 0 && content_count == 0 && remote_state.generation == last_fetched.generation {
        return Success::new(
            "fetch",
            json!({ "up_to_date": true, "generation": local_head.generation }),
            "already up to date\n".to_string(),
        );
    }

    // Update local head to match remote.
    let new_head = wheat_store::RepositoryHead {
        generation: remote_state.generation,
        envelope: remote_state.envelope,
    };
    // CAS the local head: expected = current local head, proposed = remote head.
    local
        .heads
        .compare_and_advance(local_head, new_head)
        .map_err(|e| anyhow::anyhow!("failed to update local head: {e}"))?;

    // Update remote config.
    let mut updated_cfg = remote_cfg;
    updated_cfg.set_last_fetched(&new_head);
    updated_cfg.write(&context.store_root)?;

    let human = format!(
        "fetched {obj_count} objects, {content_count} content blobs\nupdated to generation {}, envelope {}\n",
        new_head.generation, new_head.envelope,
    );
    Success::new(
        "fetch",
        json!({
            "up_to_date": false,
            "generation": new_head.generation,
            "envelope": new_head.envelope.to_string(),
            "objects": obj_count,
            "content": content_count,
        }),
        human,
    )
}

fn native_publish(
    workspace: &Path,
    explicit_store: Option<&Path>,
    arguments: NativePublishArgs,
) -> Result<Success> {
    let context = resolve_context(workspace, explicit_store)?;
    let local = open(&context.store_root)?;
    let _operation =
        rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
    let local_head = local.heads.read()?;

    // Resolve remote URL.
    let (parsed, mut remote_cfg) = if let Some(url) = arguments.remote.as_deref() {
        let parsed = remote::parse_wheat_url(url)?;
        // If no remote.json exists, create one (first publish of a locally-initialized repo).
        let cfg = remote_config::RemoteConfig::read(&context.store_root)?.unwrap_or_else(|| {
            remote_config::RemoteConfig::new(
                url,
                &wheat_store::RepositoryHead {
                    generation: 0,
                    envelope: wheat_core::RepositoryEnvelopeId::from_digest([0u8; 32]),
                },
            )
        });
        if !cfg.has_same_identity(&parsed) {
            return Err(CliFailure::invalid(
                "explicit native remote does not match this store's tracked remote",
            ));
        }
        (parsed, cfg)
    } else {
        resolve_remote(&context.store_root, None)?
    };

    let last_fetched = remote_cfg.last_fetched_head()?;

    let rt = runtime()?;
    let outcome = rt.block_on(async {
        let mut session = remote::connect(&parsed.ws_url).await?;
        let state = remote::open_repository(&mut session, &parsed.repo_name).await?;

        // Check that the remote hasn't advanced past what we last fetched.
        if state.generation != last_fetched.generation || state.envelope != last_fetched.envelope {
            // Special case: if this is a first publish (last_fetched is all zeros),
            // we use the remote's current state as the expected head.
            let is_first_publish =
                last_fetched.generation == 0 && *last_fetched.envelope.as_bytes() == [0u8; 32];
            if !is_first_publish {
                remote::close(&mut session).await?;
                bail!(
                    "remote has advanced (generation {} → {}); run `wheat fetch` first",
                    last_fetched.generation,
                    state.generation,
                );
            }
        }

        let expected = wheat_store::RepositoryHead {
            generation: state.generation,
            envelope: state.envelope,
        };

        let result = remote::publish(
            &mut session,
            1,
            &local.objects,
            &local.content,
            local_head.envelope,
            &expected,
            arguments.force,
        )
        .await?;

        remote::close(&mut session).await?;
        Ok::<_, anyhow::Error>(result)
    })?;

    match outcome {
        remote::PublishOutcome::Ok {
            new_generation,
            new_envelope,
        } => {
            // Update remote config to track the new remote head.
            let new_remote_head = wheat_store::RepositoryHead {
                generation: new_generation,
                envelope: new_envelope,
            };
            remote_cfg.set_last_fetched(&new_remote_head);
            if let Some(url) = arguments.remote {
                remote_cfg.set_remote(&url, &parsed);
            }
            remote_cfg.write(&context.store_root)?;

            let human =
                format!("published to generation {new_generation}, envelope {new_envelope}\n");
            Success::new(
                "publish",
                json!({
                    "status": "ok",
                    "generation": new_generation,
                    "envelope": new_envelope.to_string(),
                }),
                human,
            )
        }
        remote::PublishOutcome::Stale {
            remote_generation,
            remote_envelope,
            message,
        } => Err(CliFailure::stale(format!(
            "publish rejected: remote is at generation {remote_generation} (envelope {remote_envelope}); {message}"
        ))),
        remote::PublishOutcome::Rejected { message } => {
            Err(CliFailure::invalid(format!("publish rejected: {message}")))
        }
        remote::PublishOutcome::Error { message } => {
            Err(CliFailure::transport(format!("publish error: {message}")))
        }
    }
}

/// Resolve the remote URL from --remote flag or remote.json.
fn resolve_remote(
    store_root: &Path,
    explicit_url: Option<&str>,
) -> Result<(remote::WheatUrl, remote_config::RemoteConfig)> {
    if let Some(url) = explicit_url {
        let parsed = remote::parse_wheat_url(url)?;
        let cfg = remote_config::RemoteConfig::read(store_root)?
            .with_context(|| "no remote.json found; use wheat clone or pass --remote")?;
        if !cfg.has_same_identity(&parsed) {
            return Err(CliFailure::invalid(
                "explicit native remote does not match this store's tracked remote",
            ));
        }
        Ok((parsed, cfg))
    } else {
        let cfg = remote_config::RemoteConfig::read(store_root)?
            .with_context(|| "no remote configured; use wheat clone or pass --remote")?;
        let parsed = remote::parse_wheat_url(&cfg.url)?;
        Ok((parsed, cfg))
    }
}

fn list_workspaces(workspace: &Path, explicit_store: Option<&Path>) -> Result<Success> {
    workspace_lifecycle::list(workspace, explicit_store)
}

fn unregister_workspace(workspace: &Path, explicit_store: Option<&Path>) -> Result<Success> {
    workspace_lifecycle::unregister(workspace, explicit_store)
}

fn print_reapplied(output: &mut String, changes: &[ChangeId]) -> Result<()> {
    for change in changes {
        writeln!(output, "reapplied {change}")?;
    }
    Ok(())
}
