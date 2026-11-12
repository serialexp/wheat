//! Maintenance operations on a Wheat server, over its admin path.
//!
//! An operation is started with one request and then polled: it keeps running on the
//! server whether or not this connection stays open, so a lost connection loses only
//! the waiting. [`wait`] polls an operation that is running, on a new connection if
//! need be.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use uuid::Uuid;
use wheat_protocol::*;

use crate::remote::WheatSession;

/// How long [`wait`] leaves between asking how an operation is doing.
const POLL: Duration = Duration::from_millis(200);

/// What to ask the server to do. Bundle and source names are relative to the server's
/// exchange directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Export the fork network `repository` (a name, `owner/repo`) belongs to.
    Export { repository: String, bundle: String },
    /// Import a network, giving each repository (by its ID in the bundle) a name.
    Import {
        bundle: String,
        names: Vec<(Uuid, String)>,
    },
    /// Adopt a standalone repository directory as the repository `name`.
    Adopt { source: String, name: String },
    /// Delete the repository `repository`, with its network when it is the last in it.
    Delete { repository: String },
    /// Count what the network `repository` belongs to holds that nothing reaches. Only a
    /// dry run exists so far.
    GcDryRun { repository: String },
}

/// How an operation ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Finished {
    Succeeded(String),
    Failed(String),
}

/// A request the server answered with an Error message.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Refused {
    pub code: u16,
    pub retryable: bool,
    /// How long the server expects the refusal to last, if it said.
    pub retry_after: Option<Duration>,
    pub message: String,
}

fn command(operation: &Operation) -> AdminCommand {
    match operation {
        Operation::Export { repository, bundle } => {
            AdminCommand::AdminExportCommand(AdminExportCommandOutput {
                command_kind: 1,
                repository: repository.clone(),
                bundle: bundle.clone(),
            })
        }
        Operation::Import { bundle, names } => {
            AdminCommand::AdminImportCommand(AdminImportCommandOutput {
                command_kind: 2,
                bundle: bundle.clone(),
                names: names
                    .iter()
                    .map(|(repository, name)| AdminRepositoryName {
                        repository: Uuid128 {
                            bytes: repository.as_bytes().to_vec(),
                        },
                        name: name.clone(),
                    })
                    .collect(),
            })
        }
        Operation::Delete { repository } => {
            AdminCommand::AdminDeleteCommand(AdminDeleteCommandOutput {
                command_kind: 4,
                repository: repository.clone(),
            })
        }
        Operation::GcDryRun { repository } => AdminCommand::AdminGcCommand(AdminGcCommandOutput {
            command_kind: 5,
            repository: repository.clone(),
            dry_run: true,
        }),
        Operation::Adopt { source, name } => {
            AdminCommand::AdminAdoptCommand(AdminAdoptCommandOutput {
                command_kind: 3,
                source: source.clone(),
                name: name.clone(),
            })
        }
    }
}

fn refused(error: ErrorOutput) -> anyhow::Error {
    Refused {
        code: error.code,
        retryable: error.retryable,
        retry_after: (error.retry_after_seconds > 0)
            .then(|| Duration::from_secs(error.retry_after_seconds)),
        message: error.message,
    }
    .into()
}

/// Start `operation`; returns its ID on the server.
pub async fn start(session: &mut WheatSession, request: u64, operation: &Operation) -> Result<u64> {
    session
        .send(&WheatProtocolMessage::AdminStart(AdminStartOutput {
            message_type: 0x50,
            op_id: request,
            command: command(operation),
        }))
        .await?;
    match session.receive().await? {
        WheatProtocolMessage::AdminStarted(started) => Ok(started.operation),
        WheatProtocolMessage::Error(error) => Err(refused(error)),
        other => bail!("expected AdminStarted, got {other:?}"),
    }
}

/// How operation `operation` is doing: `None` while it runs, with its phase, and
/// how it ended once it has.
pub async fn status(
    session: &mut WheatSession,
    request: u64,
    operation: u64,
) -> Result<(String, Option<Finished>)> {
    session
        .send(&WheatProtocolMessage::AdminStatusRequest(
            AdminStatusRequestOutput {
                message_type: 0x52,
                op_id: request,
                operation,
            },
        ))
        .await?;
    match session.receive().await? {
        WheatProtocolMessage::AdminStatus(status) => Ok((
            status.phase,
            match status.state {
                AdminState::AdminRunning(_) => None,
                AdminState::AdminSucceeded(_) => Some(Finished::Succeeded(status.message)),
                AdminState::AdminFailed(_) => Some(Finished::Failed(status.message)),
            },
        )),
        WheatProtocolMessage::Error(error) => Err(refused(error)),
        other => bail!("expected AdminStatus, got {other:?}"),
    }
}

/// Poll `operation` until it ends, calling `progress` with its phase whenever that
/// changes.
pub async fn wait(
    session: &mut WheatSession,
    first_request: u64,
    operation: u64,
    mut progress: impl FnMut(&str),
) -> Result<Finished> {
    let mut request = first_request;
    let mut shown = String::new();
    loop {
        let (phase, finished) = status(session, request, operation)
            .await
            .with_context(|| format!("asking about operation {operation}"))?;
        request += 1;
        if phase != shown {
            progress(&phase);
            shown = phase;
        }
        if let Some(finished) = finished {
            return Ok(finished);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Run `operation` to its end: start it, then wait for it.
pub async fn run(
    session: &mut WheatSession,
    operation: &Operation,
    progress: impl FnMut(&str),
) -> Result<Finished> {
    let id = start(session, 1, operation).await?;
    wait(session, 2, id, progress).await
}
