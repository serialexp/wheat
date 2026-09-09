use std::{fmt::Write as _, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use wheat_core::{
    ApplicationResultRecord, ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditTransition,
    FileKind, FileValue, RevisionEdits, RevisionId, RevisionMetadata, guarded::Limits,
};
use wheat_store::{
    ApplicationMapStore, BatchObjectStore, MerkleTreeStore, MutationTarget,
    PersistedConflictResolution, PersistedRepositoryMutator, PublicationStore, RepositoryReader,
    RevisionMapStore, finalize_mutation_publication, prepare_mutation_publication,
};
mod inspection;
mod local;
mod workspace;
mod workspace_files;
use local::{
    content_store, current_conflicts, current_revisions, initialize, open, repository_summary,
    revision_with_edits, runtime,
};

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
    /// Verify the complete retained repository state.
    Validate,
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
    /// Logical changes this change depends on. Omit to build on the current working frontier.
    #[arg(long = "depends-on", conflicts_with = "independent")]
    dependencies: Vec<String>,
    /// Create a root change against the repository base instead of the current working frontier.
    #[arg(long)]
    independent: bool,
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
        Command::Status => {
            let summary = repository_summary(&cli.repository)?;
            println!("working view: {}", summary.working_view);
            println!("published view: {}", summary.published_view);
            println!("changes: {}", summary.changes);
            println!("conflicts: {}", summary.conflicts);
            Ok(())
        }
        Command::Validate => {
            let local = open(&cli.repository)?;
            let head = local.heads.read()?;
            let runtime = runtime()?;
            let state = runtime.block_on(async {
                let reader = RepositoryReader::new(&local.objects, &local.content);
                let envelope = reader.envelope(head.envelope).await?;
                reader.validate_complete(envelope.state).await?;
                Ok::<_, wheat_store::RepositoryPublicationError>(envelope.state)
            })?;
            println!("valid state: {state}");
            Ok(())
        }
        Command::Log => {
            let mut output = String::new();
            for (change, revision) in current_revisions(&cli.repository)? {
                writeln!(output, "{change} {revision}")?;
            }
            print!("{output}");
            Ok(())
        }
        Command::Conflicts => {
            let mut output = String::new();
            inspection::conflicts(&current_conflicts(&cli.repository)?, &mut output)?;
            print!("{output}");
            Ok(())
        }
        Command::Edits { revision } => {
            let revision = RevisionId::from_str(&revision).context("invalid revision ID")?;
            let (revision, edits) = revision_with_edits(&cli.repository, revision)?;
            let mut output = String::new();
            inspection::edits(&revision, &edits, &mut output)?;
            print!("{output}");
            Ok(())
        }
        Command::Add(arguments) => {
            let local = open(&cli.repository)?;
            let expected = local.heads.read()?;
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
            let runtime = runtime()?;
            let objects = BatchObjectStore::new(&local.objects, 1024, 8 * 1024 * 1024);
            let edits = runtime.block_on(workspace::author_from_workspace(
                &cli.repository,
                &objects,
                &local.content,
                expected,
                change,
                predecessor,
                &arguments.paths,
                &guards,
            ))?;
            let mutator = PersistedRepositoryMutator::new(&objects, &local.content, &local.heads)
                .with_target(MutationTarget::PublishWorking);
            let (prepared, outcome) = runtime.block_on(mutator.revise(
                expected,
                change,
                predecessor,
                RevisionMetadata::default(),
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
            writeln!(output, "revision {}", outcome.revision_id)?;
            print_reapplied(&mut output, &outcome.reapplied)?;
            print!("{output}");
            Ok(())
        }
        Command::Cat { content } => {
            let id = ContentId::from_str(&content).context("invalid content ID")?;
            let bytes = content_store(&cli.repository)?.get(id)?;
            use std::io::Write as _;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }
        Command::Change(ChangeCommand::New(arguments)) => {
            let local = open(&cli.repository)?;
            let expected = local.heads.read()?;
            let dependencies = arguments
                .dependencies
                .iter()
                .map(|value| ChangeId::from_str(value).context("invalid dependency change ID"))
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            let use_frontier = dependencies.is_empty() && !arguments.independent;
            let edits = capture_initial_edit(&local.content, arguments.edit)?;
            let change = ChangeId::new();
            let proposal = ChangeProposal {
                id: change,
                metadata: ChangeMetadata::default(),
                dependencies,
                revision_metadata: RevisionMetadata::default(),
                edits,
            };
            let runtime = runtime()?;
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
            let mut output = String::new();
            writeln!(output, "change {change}")?;
            writeln!(output, "revision {}", outcome.revision_id)?;
            print_reapplied(&mut output, &outcome.reapplied)?;
            print!("{output}");
            Ok(())
        }
        Command::Resolve(arguments) => resolve(&cli.repository, arguments),
    }
}

fn resolve(root: &std::path::Path, arguments: ResolveArgs) -> Result<()> {
    let local = open(root)?;
    let expected = local.heads.read()?;
    let revision = RevisionId::from_str(&arguments.revision).context("invalid revision ID")?;
    let runtime = runtime()?;
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
            bail!("revision is not the current conflicted application");
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
            bail!("revision is not the current conflicted application");
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
        None => bail!("a value or --delete is required"),
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
    print!("{output}");
    Ok(())
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
