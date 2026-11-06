//! Maintenance operations: export a fork network to a bundle, import one, adopt a
//! standalone repository.
//!
//! An admin session ([`wheat_protocol::ADMIN_PATH`]) starts them with `AdminStart`.
//! An operation runs on a task of its own and outlives the request and the session
//! that started it: `AdminStarted` names it, and `AdminStatusRequest`, on this session
//! or another, says how it is going and what it produced. Only one runs at a time,
//! since each holds a network still, or the content store, for stretches the next
//! should not interleave with; another start is refused with a retryable error naming
//! the one running. The last 64 finished operations stay for asking about.
//!
//! The operations name bundles and standalone repositories by a single path segment
//! under [`ServerConfig::exchange_dir`](crate::server::ServerConfig::exchange_dir), so
//! an admin cannot reach the rest of the server's disk by them.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::task::spawn_blocking;
use uuid::Uuid;
use wheat_protocol::{AdminCommand, AdminRepositoryName, Uuid128};
use wheat_store::RepositoryDirectory;

use crate::error_text::chain;
use crate::registry::{DirectoryHandle, Registry};

/// Finished operations kept for asking about.
const KEPT: usize = 64;
/// How long an export waits for the publishes writing to its network to finish.
const QUIESCE_WAIT: Duration = Duration::from_secs(60);

/// Where an operation is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Running,
    Succeeded,
    Failed,
}

/// How an operation is going.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationStatus {
    pub state: RunState,
    /// What a running operation is doing.
    pub phase: String,
    /// What a finished operation produced, or why it failed.
    pub message: String,
}

/// The operations of one server.
#[derive(Default)]
pub struct Operations {
    state: Mutex<OperationTable>,
}

#[derive(Default)]
struct OperationTable {
    last: u64,
    running: Option<u64>,
    finished: VecDeque<u64>,
    by_id: HashMap<u64, OperationStatus>,
}

impl Operations {
    /// How operation `id` is going, or `None` when there was none or it is no longer
    /// kept.
    pub fn status(&self, id: u64) -> Option<OperationStatus> {
        self.lock().by_id.get(&id).cloned()
    }

    /// Starts an operation, or says which one is running.
    fn begin(&self) -> Result<u64, StartError> {
        let mut table = self.lock();
        if let Some(running) = table.running {
            return Err(StartError::Busy(running));
        }
        table.last += 1;
        let id = table.last;
        table.running = Some(id);
        table.by_id.insert(
            id,
            OperationStatus {
                state: RunState::Running,
                phase: "starting".to_owned(),
                message: String::new(),
            },
        );
        Ok(id)
    }

    fn set_phase(&self, id: u64, phase: &str) {
        if let Some(status) = self.lock().by_id.get_mut(&id) {
            phase.clone_into(&mut status.phase);
        }
    }

    fn finish(&self, id: u64, result: Result<String, String>) {
        let mut table = self.lock();
        if let Some(status) = table.by_id.get_mut(&id) {
            status.phase.clear();
            (status.state, status.message) = match result {
                Ok(message) => (RunState::Succeeded, message),
                Err(message) => (RunState::Failed, message),
            };
        }
        table.running = None;
        table.finished.push_back(id);
        while table.finished.len() > KEPT {
            if let Some(oldest) = table.finished.pop_front() {
                table.by_id.remove(&oldest);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, OperationTable> {
        // A panic while holding the lock leaves the table as consistent as before it.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Why an operation was not started.
#[derive(Debug, Eq, PartialEq)]
pub enum StartError {
    /// The server cannot run this: it serves no repositories directory, or has no
    /// exchange directory.
    Unsupported(&'static str),
    /// The command is malformed.
    Invalid(String),
    /// Operation `0` is running.
    Busy(u64),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Unsupported(reason) => f.write_str(reason),
            StartError::Invalid(reason) => f.write_str(reason),
            StartError::Busy(running) => {
                write!(
                    f,
                    "operation {running} is running; try again when it is done"
                )
            }
        }
    }
}

impl Error for StartError {}

/// What to run, with its arguments checked.
enum Job {
    Export {
        repository: String,
        bundle: PathBuf,
    },
    Import {
        bundle: PathBuf,
        names: Vec<(Uuid, String)>,
    },
    Adopt {
        source: PathBuf,
        name: String,
    },
    Delete {
        repository: String,
    },
}

/// Starts `command` on a task of its own and returns its operation ID. The caller has
/// checked the principal is an admin.
pub fn start(
    registry: &Arc<Registry>,
    exchange: Option<&Path>,
    command: AdminCommand,
) -> Result<u64, StartError> {
    let directory = registry.directory().ok_or(StartError::Unsupported(
        "this server does not serve a repositories directory (--repos-dir)",
    ))?;
    let job = parse(exchange, command)?;
    let id = registry.operations().begin()?;
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        // Its own task, so a panic ends the operation instead of leaving it running.
        let work = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move { run(&registry, id, directory, job).await }
        });
        let result = match work.await {
            Ok(result) => result,
            Err(panic) => Err(format!("the operation panicked: {panic}")),
        };
        registry.operations().finish(id, result);
    });
    Ok(id)
}

fn parse(exchange: Option<&Path>, command: AdminCommand) -> Result<Job, StartError> {
    let exchange = || {
        exchange.ok_or(StartError::Unsupported(
            "this server has no exchange directory (--exchange-dir)",
        ))
    };
    match command {
        AdminCommand::AdminExportCommand(command) => Ok(Job::Export {
            repository: command.repository,
            bundle: exchange_path(exchange()?, &command.bundle)?,
        }),
        AdminCommand::AdminImportCommand(command) => Ok(Job::Import {
            bundle: exchange_path(exchange()?, &command.bundle)?,
            names: command
                .names
                .into_iter()
                .map(|AdminRepositoryName { repository, name }| Ok((uuid(&repository)?, name)))
                .collect::<Result<_, StartError>>()?,
        }),
        AdminCommand::AdminAdoptCommand(command) => Ok(Job::Adopt {
            source: exchange_path(exchange()?, &command.source)?,
            name: command.name,
        }),
        AdminCommand::AdminDeleteCommand(command) => Ok(Job::Delete {
            repository: command.repository,
        }),
    }
}

fn uuid(id: &Uuid128) -> Result<Uuid, StartError> {
    Uuid::from_slice(&id.bytes)
        .map_err(|_| StartError::Invalid("a repository ID is not 16 bytes".into()))
}

/// `name`, one path segment, in the exchange directory.
fn exchange_path(exchange: &Path, name: &str) -> Result<PathBuf, StartError> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', '\0']) {
        return Err(StartError::Invalid(format!(
            "{name:?} is not a name in the exchange directory: it is one path segment, not starting with '.'"
        )));
    }
    Ok(exchange.join(name))
}

async fn run(
    registry: &Arc<Registry>,
    id: u64,
    directory: DirectoryHandle,
    job: Job,
) -> Result<String, String> {
    let operations = registry.operations();
    let phase = |phase: &str| operations.set_phase(id, phase);
    match job {
        Job::Export { repository, bundle } => {
            export(registry, &directory, &repository, bundle, &phase).await
        }
        Job::Import { bundle, names } => {
            phase("importing");
            let content = directory.content.content_store();
            let repositories = directory.repositories.clone();
            let imported = blocking(move || async move {
                repositories
                    .import_network(&bundle, &names, &content)
                    .await
                    .map_err(|error| chain(&error))
            })
            .await?;
            let mut message = format!(
                "imported network {}{}\n",
                imported.network,
                if imported.resumed {
                    " (it was already there; the names are written now)"
                } else {
                    ""
                }
            );
            for (name, placed) in &imported.repositories {
                message.push_str(&format!(
                    "repository {} {name}\n",
                    placed.location.repository
                ));
            }
            Ok(message)
        }
        Job::Delete { repository } => {
            phase("deleting");
            registry
                .delete(&repository)
                .await
                .map_err(|error| format!("{repository}: {error}"))?;
            Ok(format!("deleted {repository}\n"))
        }
        Job::Adopt { source, name } => {
            phase("adopting");
            let content = directory.content.content_store();
            let repositories = directory.repositories.clone();
            let placed = blocking({
                let name = name.clone();
                move || async move {
                    repositories
                        .adopt_repository(&RepositoryDirectory::new(source), &name, &content)
                        .await
                        .map_err(|error| chain(&error))
                }
            })
            .await?;
            Ok(format!(
                "adopted {name} as repository {} of network {}\n",
                placed.location.repository, placed.location.network
            ))
        }
    }
}

async fn export(
    registry: &Arc<Registry>,
    directory: &DirectoryHandle,
    repository: &str,
    bundle: PathBuf,
    phase: &(dyn Fn(&str) + Sync),
) -> Result<String, String> {
    let repositories = directory.repositories.clone();
    let located = {
        let repositories = repositories.clone();
        let repository = repository.to_owned();
        spawn_blocking(move || repositories.locate(&repository))
            .await
            .map_err(|error| chain(&error))?
            .map_err(|error| chain(&error))?
    };
    let Some(location) = located else {
        return Err(format!("there is no repository {repository}"));
    };

    phase("holding the network still");
    let held = tokio::time::timeout(QUIESCE_WAIT, registry.quiesce(repository))
        .await
        .map_err(|_| {
            format!(
                "publishes to {repository} did not finish within {} seconds",
                QUIESCE_WAIT.as_secs()
            )
        })?
        .map_err(|error| chain(&error))?
        .ok_or_else(|| format!("there is no repository {repository}"))?;
    phase("copying the structure");
    let export = {
        let repositories = repositories.clone();
        // Publishes refused meanwhile are told how long the copy should take.
        let sized = {
            let repositories = repositories.clone();
            spawn_blocking(move || repositories.network(location.network).files())
                .await
                .map_err(|error| chain(&error))?
        };
        if let Ok(files) = sized {
            held.expect(copy_estimate(files.iter().map(|file| file.len).sum()));
        }
        spawn_blocking(move || repositories.export_network(location.network, bundle))
            .await
            .map_err(|error| chain(&error))?
            .map_err(|error| chain(&error))?
    };
    // What follows reads only the copy and the content store, which only grows.
    drop(held);

    phase("copying the content");
    let content = directory.content.content_store();
    let manifest =
        blocking(
            move || async move { export.finish(&content).await.map_err(|error| chain(&error)) },
        )
        .await?;

    let names = repositories
        .names_in_network(manifest.network)
        .map_err(|error| chain(&error))?;
    let mut message = format!(
        "exported network {}: {} contents, {} bytes\n",
        manifest.network, manifest.content_count, manifest.content_bytes
    );
    for id in &manifest.repositories {
        let name = names
            .iter()
            .find(|(_, location)| location.repository == *id)
            .map_or("(no name)", |(name, _)| name.as_str());
        message.push_str(&format!("repository {id} {name}\n"));
    }
    Ok(message)
}

/// How long copying `bytes` of a network's files is expected to take, for the clients
/// whose publishes it refuses: at a rate well under a disk's, so the estimate errs long.
fn copy_estimate(bytes: u64) -> Duration {
    const BYTES_PER_SECOND: u64 = 50 * 1024 * 1024;
    Duration::from_secs(bytes.div_ceil(BYTES_PER_SECOND).max(1))
}

/// Runs `work`, which blocks, on a thread for blocking work, driving the future it
/// returns there.
async fn blocking<F, Fut, T>(work: F) -> Result<T, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>>,
    T: Send + 'static,
{
    crate::blocking::drive(work)
        .await
        .map_err(|error| chain(&error))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_operation_runs_at_a_time() {
        let operations = Operations::default();
        let first = operations.begin().unwrap();
        assert!(matches!(operations.begin(), Err(StartError::Busy(id)) if id == first));
        operations.set_phase(first, "copying");
        assert_eq!(operations.status(first).unwrap().phase, "copying");

        operations.finish(first, Ok("done".to_owned()));
        let status = operations.status(first).unwrap();
        assert_eq!(status.state, RunState::Succeeded);
        assert_eq!(status.message, "done");

        let second = operations.begin().unwrap();
        assert_ne!(first, second);
        operations.finish(second, Err("broke".to_owned()));
        assert_eq!(operations.status(second).unwrap().state, RunState::Failed);
        assert!(operations.status(second + 1).is_none());
    }

    #[test]
    fn finished_operations_are_kept_only_so_long() {
        let operations = Operations::default();
        let first = operations.begin().unwrap();
        operations.finish(first, Ok(String::new()));
        for _ in 0..KEPT {
            let next = operations.begin().unwrap();
            operations.finish(next, Ok(String::new()));
        }
        assert!(operations.status(first).is_none());
    }

    #[test]
    fn a_copy_is_expected_to_take_whole_seconds_and_at_least_one() {
        assert_eq!(copy_estimate(0), Duration::from_secs(1));
        assert_eq!(copy_estimate(50 * 1024 * 1024), Duration::from_secs(1));
        assert_eq!(copy_estimate(50 * 1024 * 1024 + 1), Duration::from_secs(2));
    }

    #[test]
    fn exchange_names_are_one_plain_segment() {
        let exchange = Path::new("/exchange");
        assert_eq!(
            exchange_path(exchange, "bundle").unwrap(),
            Path::new("/exchange/bundle")
        );
        for refused in ["", ".", "..", ".hidden", "a/b", "a\\b", "/abs", "../out"] {
            assert!(exchange_path(exchange, refused).is_err(), "{refused:?}");
        }
    }
}
