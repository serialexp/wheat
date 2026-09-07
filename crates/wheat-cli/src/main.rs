use std::{fmt::Write as _, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use wheat_core::{
    ApplicationContext, ChangeId, ConflictResolution, ContentId, EditTransition, FileKind,
    FileValue, RevisionEdits, RevisionId, guarded::Limits,
};
mod inspection;
mod local;
mod workspace;
mod workspace_files;
use local::{content_store, initialize, with_repository};

#[derive(Debug, Parser)]
#[command(name = "wheat", about = "Wheat source control prototype")]
struct Cli {
    /// Repository directory to operate on.
    #[arg(long, default_value = ".")]
    repository: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize an empty native repository.
    Init,
    /// Show the current native repository state.
    Status,
    /// Inspect logical changes and immutable revisions.
    Log,
    /// Inspect durable conflicts and per-edit failure evidence.
    Conflicts,
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
}

#[derive(Debug, Subcommand)]
enum ChangeCommand {
    /// Create a logical change.
    New(NewArgs),
}

#[derive(Debug, Args)]
struct NewArgs {
    /// Logical changes this change depends on.
    #[arg(long = "depends-on")]
    dependencies: Vec<String>,
    #[command(flatten)]
    edit: EditArgs,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Stable logical change identity.
    #[arg(long)]
    change: String,
    /// Exact current immutable revision.
    #[arg(long)]
    predecessor: String,
    /// Explicit repository-relative workspace paths. No other files are read or written.
    #[arg(required = true)]
    paths: Vec<String>,
    /// Wider guard as PATH:START:END, using byte offsets in canonical-LF workspace content.
    #[arg(long = "guard", value_parser = parse_guard)]
    guards: Vec<GuardRange>,
}

#[derive(Clone, Debug)]
struct GuardRange {
    path: String,
    range: std::ops::Range<usize>,
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
    path: String,
    /// Expected current UTF-8 content; omit when creating a path.
    #[arg(long)]
    expected: Option<String>,
    /// Expected file kind (defaults to regular).
    #[arg(long, value_enum)]
    expected_kind: Option<CliFileKind>,
    /// Replacement UTF-8 content; omit with --delete or for a mode-only edit.
    #[arg(
        long,
        conflicts_with = "delete",
        required_unless_present_any = ["delete", "replacement_kind"]
    )]
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
struct ResolveArgs {
    /// Conflicted immutable revision.
    revision: String,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => initialize(&cli.repository),
        Command::Status => with_repository(&cli.repository, false, |repository, _, output| {
            let working = repository.working_view();
            writeln!(output, "working view: {}", working.id)?;
            writeln!(output, "published view: {}", repository.published_view().id)?;
            writeln!(
                output,
                "changes: {}",
                repository.current_revisions().count()
            )?;
            writeln!(output, "conflicts: {}", working.unresolved_conflicts.len())?;
            Ok(())
        }),
        Command::Log => with_repository(&cli.repository, false, |repository, _, output| {
            for (change, revision) in repository.current_revisions() {
                writeln!(output, "{change} {revision}")?;
            }
            Ok(())
        }),
        Command::Conflicts => with_repository(&cli.repository, false, |repository, _, output| {
            inspection::conflicts(repository, output)
        }),
        Command::Edits { revision } => {
            with_repository(&cli.repository, false, |repository, _, output| {
                let revision = RevisionId::from_str(&revision).context("invalid revision ID")?;
                inspection::edits(repository, revision, output)
            })
        }
        Command::Add(arguments) => {
            with_repository(&cli.repository, true, |repository, store, output| {
                let change = ChangeId::from_str(&arguments.change).context("invalid change ID")?;
                let predecessor = RevisionId::from_str(&arguments.predecessor)
                    .context("invalid predecessor revision ID")?;
                let mut guards = std::collections::BTreeMap::new();
                for guard in arguments.guards {
                    guards
                        .entry(guard.path)
                        .or_insert_with(Vec::new)
                        .push(guard.range);
                }
                let edits = workspace::author_from_workspace(
                    &cli.repository,
                    repository,
                    store,
                    change,
                    predecessor,
                    &arguments.paths,
                    &guards,
                )?;
                let outcome = repository.revise_change(
                    change,
                    predecessor,
                    edits,
                    ApplicationContext::new(store),
                )?;
                writeln!(output, "revision {}", outcome.revision_id)?;
                print_reapplied(output, &outcome.reapplied)
            })
        }
        Command::Cat { content } => {
            let id = ContentId::from_str(&content).context("invalid content ID")?;
            let bytes = content_store(&cli.repository).get(id)?;
            use std::io::Write as _;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }
        Command::Change(ChangeCommand::New(arguments)) => {
            with_repository(&cli.repository, true, |repository, store, output| {
                let dependencies = arguments
                    .dependencies
                    .iter()
                    .map(|value| ChangeId::from_str(value).context("invalid dependency change ID"))
                    .collect::<Result<Vec<_>>>()?;
                let edits = capture_initial_edit(store, arguments.edit)?;
                let (change, outcome) = repository.create_change(
                    dependencies,
                    edits,
                    ApplicationContext::new(store),
                )?;
                writeln!(output, "change {change}")?;
                writeln!(output, "revision {}", outcome.revision_id)?;
                print_reapplied(output, &outcome.reapplied)?;
                Ok(())
            })
        }
        Command::Resolve(arguments) => {
            with_repository(&cli.repository, true, |repository, store, output| {
                let revision =
                    RevisionId::from_str(&arguments.revision).context("invalid revision ID")?;
                let target = repository
                    .conflicted_application(revision)
                    .context("revision is not the current conflicted application")?
                    .target_view;
                let mut tree = repository
                    .view(target)
                    .expect("application target view exists")
                    .tree
                    .clone();
                match arguments.value {
                    Some(value) => {
                        let kind = arguments
                            .kind
                            .map(Into::into)
                            .or_else(|| tree.get(&arguments.path).map(|value| value.kind))
                            .unwrap_or(FileKind::Regular);
                        let content =
                            put_captured_value(&content_store(&cli.repository), &value, kind)?;
                        tree.insert(arguments.path, FileValue { content, kind });
                    }
                    None if arguments.delete => {
                        tree.remove(&arguments.path);
                    }
                    None => bail!("a value or --delete is required"),
                }
                let outcome = repository.resolve_conflict(
                    ConflictResolution {
                        conflict_revision: revision,
                        resolved_tree: tree,
                    },
                    ApplicationContext::new(store),
                )?;
                writeln!(output, "revision {}", outcome.revision_id)?;
                print_reapplied(output, &outcome.reapplied)?;
                Ok(())
            })
        }
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
    let captured = wheat_core::edit_capture::capture_edit(
        &arguments.path,
        before,
        after,
        None,
        Limits::default(),
    )?;
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
