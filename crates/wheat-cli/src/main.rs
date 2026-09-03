use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use wheat_core::{
    ApplicationResult, ChangeId, ConflictResolution, ContentId, FileEdit, Repository, RevisionId,
    Transformation, Tree,
};

const STATE_DIR: &str = ".wheat";
const STATE_FILE: &str = "repository.yaml";

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
    /// Inspect durable conflicts.
    Conflicts,
    /// Create or revise a logical change.
    #[command(subcommand)]
    Change(ChangeCommand),
    /// Resolve a durable conflict with an exact tree entry.
    Resolve(ResolveArgs),
}

#[derive(Debug, Subcommand)]
enum ChangeCommand {
    /// Create a logical change.
    New(NewArgs),
    /// Create an immutable revision of a logical change.
    Revise(ReviseArgs),
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
struct ReviseArgs {
    /// Stable logical change identity.
    change: String,
    /// Current immutable revision of the change.
    #[arg(long)]
    predecessor: String,
    #[command(flatten)]
    edit: EditArgs,
}

#[derive(Debug, Args)]
struct EditArgs {
    /// Repository-relative path to edit.
    #[arg(long)]
    path: String,
    /// Expected current UTF-8 content; omit when creating a path.
    #[arg(long)]
    expected: Option<String>,
    /// Replacement UTF-8 content; omit with --delete.
    #[arg(long, conflicts_with = "delete", required_unless_present = "delete")]
    value: Option<String>,
    /// Delete the path.
    #[arg(long)]
    delete: bool,
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
    /// Resolve by deleting the path.
    #[arg(long)]
    delete: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => initialize(&cli.repository),
        Command::Status => with_repository(&cli.repository, false, |repository| {
            println!("working view: {}", repository.working_view().id);
            println!("published view: {}", repository.published_view().id);
            println!("changes: {}", repository.current_revisions().count());
            println!("conflicts: {}", repository.conflicts().count());
            Ok(())
        }),
        Command::Log => with_repository(&cli.repository, false, |repository| {
            for (change, revision) in repository.current_revisions() {
                println!("{change} {revision}");
            }
            Ok(())
        }),
        Command::Conflicts => with_repository(&cli.repository, false, |repository| {
            for application in repository.conflicts() {
                let ApplicationResult::Conflicted { conflict, .. } = &application.result else {
                    unreachable!()
                };
                println!(
                    "revision {} on {}",
                    application.revision_id, application.target_view
                );
                for path in &conflict.paths {
                    println!(
                        "  {} expected={:?} actual={:?} proposed={:?}",
                        path.path, path.expected, path.actual, path.proposed
                    );
                }
            }
            Ok(())
        }),
        Command::Change(ChangeCommand::New(arguments)) => {
            with_repository(&cli.repository, true, |repository| {
                let dependencies = arguments
                    .dependencies
                    .iter()
                    .map(|value| ChangeId::from_str(value).context("invalid dependency change ID"))
                    .collect::<Result<Vec<_>>>()?;
                let (change, outcome) =
                    repository.create_change(dependencies, arguments.edit.into())?;
                println!("change {change}");
                println!("revision {}", outcome.revision_id);
                print_reapplied(&outcome.reapplied);
                Ok(())
            })
        }
        Command::Change(ChangeCommand::Revise(arguments)) => {
            with_repository(&cli.repository, true, |repository| {
                let change = ChangeId::from_str(&arguments.change).context("invalid change ID")?;
                let predecessor = RevisionId::from_str(&arguments.predecessor)
                    .context("invalid predecessor revision ID")?;
                let outcome =
                    repository.revise_change(change, predecessor, arguments.edit.into())?;
                println!("revision {}", outcome.revision_id);
                print_reapplied(&outcome.reapplied);
                Ok(())
            })
        }
        Command::Resolve(arguments) => with_repository(&cli.repository, true, |repository| {
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
                    tree.insert(arguments.path, ContentId::of(value.as_bytes()));
                }
                None if arguments.delete => {
                    tree.remove(&arguments.path);
                }
                None => bail!("a value or --delete is required"),
            }
            let outcome = repository.resolve_conflict(ConflictResolution {
                conflict_revision: revision,
                resolved_tree: tree,
            })?;
            println!("revision {}", outcome.revision_id);
            print_reapplied(&outcome.reapplied);
            Ok(())
        }),
    }
}

impl From<EditArgs> for Transformation {
    fn from(arguments: EditArgs) -> Self {
        Self {
            edits: vec![FileEdit {
                path: arguments.path,
                expected: arguments
                    .expected
                    .as_deref()
                    .map(|value| ContentId::of(value.as_bytes())),
                replacement: arguments
                    .value
                    .as_deref()
                    .map(|value| ContentId::of(value.as_bytes())),
            }],
        }
    }
}

fn initialize(root: &Path) -> Result<()> {
    let path = state_path(root);
    if path.exists() {
        bail!("Wheat repository already exists at {}", path.display());
    }
    fs::create_dir_all(path.parent().expect("state path has parent"))?;
    save(&path, &Repository::new(Tree::new()))?;
    println!(
        "initialized Wheat repository in {}",
        path.parent().unwrap().display()
    );
    Ok(())
}

fn with_repository(
    root: &Path,
    persist: bool,
    operation: impl FnOnce(&mut Repository) -> Result<()>,
) -> Result<()> {
    let path = state_path(root);
    let bytes =
        fs::read(&path).with_context(|| format!("not a Wheat repository: {}", root.display()))?;
    let mut repository =
        serde_yaml::from_slice(&bytes).context("invalid Wheat repository state")?;
    operation(&mut repository)?;
    if persist {
        save(&path, &repository)?;
    }
    Ok(())
}

fn save(path: &Path, repository: &Repository) -> Result<()> {
    let temporary = path.with_extension("yaml.tmp");
    fs::write(&temporary, serde_yaml::to_string(repository)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(STATE_FILE)
}

fn print_reapplied(changes: &[ChangeId]) {
    for change in changes {
        println!("reapplied {change}");
    }
}
