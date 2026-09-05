use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use wheat_core::{
    ChangeId, ConflictResolution, ContentId, FileEdit, Repository, RevisionId, Transformation, Tree,
};
use wheat_store::ContentStore;

const STATE_DIR: &str = ".wheat";
const STATE_FILE: &str = "repository.yaml";
const CONTENT_DIR: &str = "objects";

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
    /// Print retained content by its native identity.
    Cat { content: String },
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
            let working = repository.working_view();
            println!("working view: {}", working.id);
            println!("published view: {}", repository.published_view().id);
            println!("changes: {}", repository.current_revisions().count());
            println!("conflicts: {}", working.unresolved_conflicts.len());
            Ok(())
        }),
        Command::Log => with_repository(&cli.repository, false, |repository| {
            for (change, revision) in repository.current_revisions() {
                println!("{change} {revision}");
            }
            Ok(())
        }),
        Command::Conflicts => with_repository(&cli.repository, false, |repository| {
            let working = repository.working_view().id;
            for conflict in repository.conflicts_in_view(working)? {
                println!("conflict {} on {}", conflict.id, conflict.base_view);
                for path in &conflict.paths {
                    println!(
                        "  {} expected={:?} actual={:?} proposed={:?}",
                        path.path, path.expected, path.actual, path.proposed
                    );
                }
            }
            Ok(())
        }),
        Command::Cat { content } => {
            let id = ContentId::from_str(&content).context("invalid content ID")?;
            let bytes = content_store(&cli.repository).get(id)?;
            use std::io::Write as _;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }
        Command::Change(ChangeCommand::New(arguments)) => {
            with_repository(&cli.repository, true, |repository| {
                let dependencies = arguments
                    .dependencies
                    .iter()
                    .map(|value| ChangeId::from_str(value).context("invalid dependency change ID"))
                    .collect::<Result<Vec<_>>>()?;
                let transformation = transformation(&cli.repository, arguments.edit)?;
                let (change, outcome) = repository.create_change(dependencies, transformation)?;
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
                let transformation = transformation(&cli.repository, arguments.edit)?;
                let outcome = repository.revise_change(change, predecessor, transformation)?;
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
                    let content = content_store(&cli.repository).put(value.as_bytes())?;
                    tree.insert(arguments.path, content);
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

fn transformation(root: &Path, arguments: EditArgs) -> Result<Transformation> {
    let store = content_store(root);
    let expected = arguments
        .expected
        .as_deref()
        .map(|value| store.put(value.as_bytes()))
        .transpose()?;
    let replacement = arguments
        .value
        .as_deref()
        .map(|value| store.put(value.as_bytes()))
        .transpose()?;
    Ok(Transformation {
        edits: vec![FileEdit {
            path: arguments.path,
            expected,
            replacement,
        }],
    })
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
    let mut repository: Repository =
        serde_yaml::from_slice(&bytes).context("invalid Wheat repository state")?;
    repository
        .validate()
        .context("invalid Wheat repository invariants")?;
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

fn content_store(root: &Path) -> ContentStore {
    ContentStore::new(root.join(STATE_DIR).join(CONTENT_DIR))
}

fn print_reapplied(changes: &[ChangeId]) {
    for change in changes {
        println!("reapplied {change}");
    }
}
