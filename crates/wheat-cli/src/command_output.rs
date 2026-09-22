//! Versioned, bounded terminal outcomes for human and automated callers.

use std::{fmt, io::Write as _, process::ExitCode};

use anyhow::Error;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use wheat_core::RepositoryError;
use wheat_git::{
    AtomicImportError, ImportError, StagedImportError, bridge::BridgeError, export::ExportError,
    push::PushError,
};
use wheat_store::{
    ContentStoreError, ObjectStoreError, PublicationError, RepositoryMutationError,
    RepositoryPublicationError, RepositoryRewriteError,
};

use crate::workspace_files::WorkspaceFileError;

pub(crate) const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_MESSAGE_BYTES: usize = 64 * 1024;
const FORMAT: &str = "wheat-command-result";
const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(Debug, Error)]
pub(crate) enum CliFailure {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    #[error("Wheat is currently supported only on Linux")]
    UnsupportedPlatform,
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    StaleState(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    LimitExceeded(String),
    #[error("{0}")]
    CorruptState(String),
    #[error("{0}")]
    TransportFailure(String),
}

impl CliFailure {
    pub(crate) fn invalid(message: impl Into<String>) -> Error {
        Self::InvalidInput(message.into()).into()
    }

    pub(crate) fn stale(message: impl Into<String>) -> Error {
        Self::StaleState(message.into()).into()
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Error {
        Self::Conflict(message.into()).into()
    }

    pub(crate) fn limit(message: impl Into<String>) -> Error {
        Self::LimitExceeded(message.into()).into()
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Error {
        Self::CorruptState(message.into()).into()
    }

    pub(crate) fn transport(message: impl Into<String>) -> Error {
        Self::TransportFailure(message.into()).into()
    }
}

pub(crate) struct Success {
    pub(crate) command: &'static str,
    pub(crate) result: Value,
    human: Vec<u8>,
}

impl Success {
    pub(crate) fn new(
        command: &'static str,
        result: impl Serialize,
        human: impl Into<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            command,
            result: serde_json::to_value(result)?,
            human: human.into(),
        })
    }

    pub(crate) fn raw(
        command: &'static str,
        result: impl Serialize,
        human: Vec<u8>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            command,
            result: serde_json::to_value(result)?,
            human,
        })
    }
}

#[derive(Clone, Copy)]
struct Classification {
    category: &'static str,
    code: &'static str,
    retryable: bool,
    exit: u8,
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    format: &'static str,
    version: u32,
    ok: bool,
    command: &'a str,
    result: &'a Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    format: &'static str,
    version: u32,
    ok: bool,
    command: &'a str,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    category: &'static str,
    code: &'static str,
    retryable: bool,
    message: &'a str,
}

pub(crate) fn emit_success(format: OutputFormat, success: &Success) -> ExitCode {
    if format == OutputFormat::Human {
        if success.human.len() > MAX_OUTPUT_BYTES {
            return emit_error(
                format,
                success.command,
                &CliFailure::LimitExceeded("command output exceeds the output limit".into()).into(),
            );
        }
        return commit_stdout(format, &success.human);
    }
    let envelope = SuccessEnvelope {
        format: FORMAT,
        version: VERSION,
        ok: true,
        command: success.command,
        result: &success.result,
    };
    match render_json(&envelope) {
        Ok(bytes) => commit_stdout(format, &bytes),
        Err(error) => emit_error(OutputFormat::Json, success.command, &error),
    }
}

pub(crate) fn emit_error(format: OutputFormat, command: &str, error: &Error) -> ExitCode {
    let classification = classify(error);
    let message = bounded_message(error);
    if format == OutputFormat::Human {
        eprintln!("error: {message}");
    } else {
        let envelope = ErrorEnvelope {
            format: FORMAT,
            version: VERSION,
            ok: false,
            command,
            error: ErrorBody {
                category: classification.category,
                code: classification.code,
                retryable: classification.retryable,
                message: &message,
            },
        };
        match render_json(&envelope) {
            Ok(bytes) => {
                let _ = std::io::stdout().write_all(&bytes);
            }
            Err(render_error) => {
                eprintln!("error: cannot render structured Wheat error: {render_error}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::from(classification.exit)
}

pub(crate) fn require_supported_platform() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(CliFailure::UnsupportedPlatform.into())
    }
}

pub(crate) fn requested_format(args: &[std::ffi::OsString]) -> OutputFormat {
    let mut requested = OutputFormat::Human;
    let mut index = 1;
    while index < args.len() {
        let value = &args[index];
        if value == "--" {
            break;
        }
        if value == "--format=json" {
            requested = OutputFormat::Json;
        } else if value == "--format=human" {
            requested = OutputFormat::Human;
        } else if value == "--format"
            && let Some(next) = args.get(index + 1).and_then(|value| value.to_str())
        {
            match next {
                "json" => requested = OutputFormat::Json,
                "human" => requested = OutputFormat::Human,
                value if !value.starts_with('-') => {}
                _ => {
                    index += 1;
                    continue;
                }
            }
            index += 1;
        }
        index += 1;
    }
    requested
}

pub(crate) fn clap_error(format: OutputFormat, error: clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        if format == OutputFormat::Human {
            print!("{error}");
        } else {
            let success = Success::new("help", json!({ "text": error.to_string() }), Vec::new())
                .expect("help text is JSON serializable");
            return emit_success(format, &success);
        }
        return ExitCode::SUCCESS;
    }
    let message = bounded_text(error.to_string());
    if format == OutputFormat::Human {
        eprint!("{message}");
        return ExitCode::from(2);
    }
    let envelope = ErrorEnvelope {
        format: FORMAT,
        version: VERSION,
        ok: false,
        command: "parse",
        error: ErrorBody {
            category: "usage",
            code: "invalid_arguments",
            retryable: false,
            message: &message,
        },
    };
    match render_json(&envelope) {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
            ExitCode::from(2)
        }
        Err(render_error) => {
            eprintln!("error: cannot render structured Wheat error: {render_error}");
            ExitCode::from(1)
        }
    }
}

fn render_json(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() >= MAX_OUTPUT_BYTES {
        return Err(CliFailure::limit(
            "structured output exceeds the output limit",
        ));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn commit_stdout(format: OutputFormat, bytes: &[u8]) -> ExitCode {
    match std::io::stdout().write_all(bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if format == OutputFormat::Human {
                eprintln!("error: cannot write command output: {error}");
            }
            ExitCode::from(1)
        }
    }
}

fn bounded_message(error: &Error) -> String {
    bounded_text(redact_urls(&format!("{error:#}")))
}

fn redact_urls(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut remaining = message;
    while let Some(offset) = remaining
        .find("https://")
        .or_else(|| remaining.find("http://"))
    {
        redacted.push_str(&remaining[..offset]);
        let tail = &remaining[offset..];
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let (url, rest) = tail.split_at(end);
        let scheme_end = url.find("://").expect("matched URL scheme") + 3;
        redacted.push_str(&url[..scheme_end]);
        let authority_end = url[scheme_end..]
            .find(['/', '?', '#'])
            .map_or(url.len(), |index| scheme_end + index);
        let authority = &url[scheme_end..authority_end];
        if authority.contains('@') {
            redacted.push_str("[redacted]@");
            redacted.push_str(authority.rsplit_once('@').expect("contains @").1);
        } else {
            redacted.push_str(authority);
        }
        let suffix = &url[authority_end..];
        let sensitive_end = suffix.find('#').unwrap_or(suffix.len());
        if let Some(query) = suffix[..sensitive_end].find('?') {
            redacted.push_str(&suffix[..query]);
            redacted.push_str("?[redacted]");
            redacted.push_str(&suffix[sensitive_end..]);
        } else {
            redacted.push_str(suffix);
        }
        remaining = rest;
    }
    redacted.push_str(remaining);
    redacted
}

fn bounded_text(message: String) -> String {
    if message.len() <= MAX_ERROR_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = message[..end].to_owned();
    bounded.push_str(" [diagnostic truncated]");
    bounded
}

fn classify(error: &Error) -> Classification {
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<CliFailure>() {
            return match failure {
                CliFailure::UnsupportedPlatform => {
                    class("platform", "unsupported_platform", false, 7)
                }
                CliFailure::InvalidInput(_) => class("input", "invalid_input", false, 2),
                CliFailure::StaleState(_) => class("state", "stale_state", true, 3),
                CliFailure::Conflict(_) => class("conflict", "unresolved_conflict", false, 4),
                CliFailure::LimitExceeded(_) => class("limit", "limit_exceeded", false, 5),
                CliFailure::CorruptState(_) => class("integrity", "corrupt_state", false, 6),
                CliFailure::TransportFailure(_) => class("transport", "transport_failure", true, 8),
            };
        }
        if let Some(import) = cause.downcast_ref::<ImportError>() {
            return match import {
                ImportError::LimitExceeded { .. } => class("limit", "limit_exceeded", false, 5),
                ImportError::InvalidUtf8(_) | ImportError::UnsupportedSubmodule(_) => {
                    class("input", "invalid_git_import", false, 2)
                }
                ImportError::CommitCycle => class("integrity", "corrupt_git_history", false, 6),
                ImportError::Git(_) => class("input", "invalid_git_repository", false, 2),
                ImportError::Content(_) | ImportError::EditCapture(_) | ImportError::Wheat(_) => {
                    continue;
                }
            };
        }
        if let Some(import) = cause.downcast_ref::<AtomicImportError>() {
            return match import {
                AtomicImportError::MissingParent => {
                    class("input", "invalid_import_destination", false, 2)
                }
                AtomicImportError::DestinationExists(_) => {
                    class("input", "import_destination_exists", false, 2)
                }
                AtomicImportError::AlreadyLocked(_) => class("state", "import_locked", true, 3),
                AtomicImportError::Validation(_) => {
                    class("integrity", "corrupt_repository", false, 6)
                }
                AtomicImportError::Io(_)
                | AtomicImportError::Stage(_)
                | AtomicImportError::Finalize(_)
                | AtomicImportError::Store(_) => continue,
            };
        }
        if let Some(import) = cause.downcast_ref::<StagedImportError>() {
            return match import {
                StagedImportError::InvalidMapping(_, _) => {
                    class("integrity", "corrupt_import_mapping", false, 6)
                }
                StagedImportError::InvalidUtf8(_) | StagedImportError::UnsupportedSubmodule(_) => {
                    class("input", "invalid_git_import", false, 2)
                }
                StagedImportError::Git(_) => class("input", "invalid_git_repository", false, 2),
                StagedImportError::Journal(_)
                | StagedImportError::Content(_)
                | StagedImportError::EditCapture { .. }
                | StagedImportError::Writer(_) => continue,
            };
        }
        if let Some(export) = cause.downcast_ref::<ExportError>() {
            return match export {
                ExportError::ConflictedView(_) => {
                    class("conflict", "unresolved_conflict", false, 4)
                }
                ExportError::LimitExceeded { .. } => class("limit", "limit_exceeded", false, 5),
                ExportError::MissingAnchor(_) | ExportError::UnmatchedAnchor(_) => {
                    class("state", "git_anchor_mismatch", true, 3)
                }
                ExportError::Disconnected { .. } => {
                    class("input", "disconnected_history", false, 2)
                }
                ExportError::Cycle(_)
                | ExportError::AmbiguousApplication { .. }
                | ExportError::InconsistentApplication { .. }
                | ExportError::MismatchedProjectedAnchor { .. } => {
                    class("integrity", "corrupt_repository", false, 6)
                }
                ExportError::Git(_) => class("io", "git_projection_io", true, 8),
                ExportError::Repository(_)
                | ExportError::ApplicationMap(_)
                | ExportError::ConflictSet(_)
                | ExportError::Tree(_)
                | ExportError::Content(_) => continue,
            };
        }
        if let Some(bridge) = cause.downcast_ref::<BridgeError>() {
            return match bridge {
                BridgeError::UnknownRemoteTip { .. } | BridgeError::Rejected(_) => {
                    class("state", "git_projection_rejected", true, 3)
                }
                BridgeError::EmptyProjection => class("input", "empty_projection", false, 2),
                BridgeError::Initialize { .. } | BridgeError::Lock { .. } => {
                    class("io", "git_projection_io", true, 8)
                }
                BridgeError::Export(_) | BridgeError::Push(_) => continue,
            };
        }
        if let Some(push) = cause.downcast_ref::<PushError>() {
            return match push {
                PushError::InvalidDestination(_)
                | PushError::InvalidSource(_)
                | PushError::InvalidProjection(_) => {
                    class("input", "invalid_git_projection", false, 2)
                }
                PushError::Spawn { .. } | PushError::Capture { .. } => {
                    class("transport", "git_transport_failure", true, 8)
                }
                PushError::GitFailed { .. } => class("state", "git_rejected", true, 3),
                PushError::InvalidRemoteOutput(_) => {
                    class("transport", "invalid_git_response", false, 8)
                }
            };
        }
        if let Some(publication) = cause.downcast_ref::<PublicationError>() {
            return match publication {
                PublicationError::Stale { .. } => {
                    class("state", "repository_head_changed", true, 3)
                }
                PublicationError::Missing
                | PublicationError::CorruptHead
                | PublicationError::InvalidGeneration => {
                    class("integrity", "corrupt_repository", false, 6)
                }
                PublicationError::Io(_) => class("io", "io_failure", true, 8),
            };
        }
        if cause
            .downcast_ref::<RepositoryMutationError>()
            .is_some_and(|mutation| matches!(mutation, RepositoryMutationError::Stale { .. }))
        {
            return class("state", "repository_head_changed", true, 3);
        }
        if let Some(rewrite) = cause.downcast_ref::<RepositoryRewriteError>() {
            return match rewrite {
                RepositoryRewriteError::StaleSelection => {
                    class("state", "stale_rewrite_selection", true, 3)
                }
                RepositoryRewriteError::InvalidBoundary
                | RepositoryRewriteError::MissingMetadata
                | RepositoryRewriteError::SemanticDrift
                | RepositoryRewriteError::UnknownAuthorityRoot => {
                    class("integrity", "invalid_repository_rewrite", false, 6)
                }
                _ => continue,
            };
        }
        if let Some(repository) = cause.downcast_ref::<RepositoryError>() {
            return match repository {
                RepositoryError::StaleRevision(_) => class("state", "stale_revision", true, 3),
                RepositoryError::NotConflicted(_) | RepositoryError::InvalidConflictResolution => {
                    class("input", "invalid_conflict_resolution", false, 2)
                }
                RepositoryError::InvalidRepositoryState(_) => {
                    class("integrity", "corrupt_repository", false, 6)
                }
                _ => class("input", "invalid_repository_operation", false, 2),
            };
        }
        if let Some(content) = cause.downcast_ref::<ContentStoreError>() {
            return match content {
                ContentStoreError::TooLarge { .. } => class("limit", "content_too_large", false, 5),
                ContentStoreError::Missing(_) => class("integrity", "missing_content", false, 6),
                ContentStoreError::Changed { .. } => class("state", "content_changed", true, 3),
                ContentStoreError::Corrupt { .. }
                | ContentStoreError::InvalidFormat { .. }
                | ContentStoreError::DeltaResolution { .. } => {
                    class("integrity", "corrupt_content", false, 6)
                }
                ContentStoreError::DeltaChainTooDeep { .. } => {
                    class("integrity", "delta_chain_too_deep", false, 6)
                }
                ContentStoreError::Io(_) => class("io", "io_failure", true, 8),
            };
        }
        if let Some(object) = cause.downcast_ref::<ObjectStoreError>() {
            return match object {
                ObjectStoreError::Missing(_) => class("integrity", "missing_object", false, 6),
                ObjectStoreError::Changed { .. } => class("state", "object_changed", true, 3),
                ObjectStoreError::WrongKind { .. }
                | ObjectStoreError::Corrupt { .. }
                | ObjectStoreError::InvalidFormat(_)
                | ObjectStoreError::InvalidEnvelope { .. }
                | ObjectStoreError::Integrity(_) => class("integrity", "corrupt_object", false, 6),
                ObjectStoreError::Backend(_) => {
                    class("transport", "storage_backend_failure", true, 8)
                }
                ObjectStoreError::Io(_) => class("io", "io_failure", true, 8),
            };
        }
        if let Some(workspace) = cause.downcast_ref::<WorkspaceFileError>() {
            return match workspace {
                WorkspaceFileError::InvalidPath
                | WorkspaceFileError::IntermediateSymlink
                | WorkspaceFileError::UnsupportedFileType => {
                    class("input", "invalid_workspace_path", false, 2)
                }
                WorkspaceFileError::TooLarge { .. } => {
                    class("limit", "workspace_file_too_large", false, 5)
                }
                WorkspaceFileError::Io(_) => class("io", "io_failure", true, 8),
            };
        }
        if let Some(publication) = cause.downcast_ref::<RepositoryPublicationError>() {
            return match publication {
                RepositoryPublicationError::MissingContent(_) => {
                    class("integrity", "missing_content", false, 6)
                }
                RepositoryPublicationError::MissingObject(_) => {
                    class("integrity", "missing_object", false, 6)
                }
                RepositoryPublicationError::InvalidReferences(_) => {
                    class("integrity", "corrupt_repository", false, 6)
                }
                _ => continue,
            };
        }
        if cause.downcast_ref::<std::io::Error>().is_some() {
            return class("io", "io_failure", true, 8);
        }
    }
    class("internal", "unexpected_failure", false, 1)
}

const fn class(
    category: &'static str,
    code: &'static str,
    retryable: bool,
    exit: u8,
) -> Classification {
    Classification {
        category,
        code,
        retryable,
        exit,
    }
}

impl fmt::Display for Success {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hint_recognizes_global_json_in_either_spelling() {
        let separate = ["wheat", "status", "--format", "json"].map(std::ffi::OsString::from);
        let joined = ["wheat", "--format=json", "status"].map(std::ffi::OsString::from);
        assert_eq!(requested_format(&separate), OutputFormat::Json);
        assert_eq!(requested_format(&joined), OutputFormat::Json);
    }

    #[test]
    fn typed_failures_have_stable_classes() {
        let error = CliFailure::StaleState("changed".into()).into();
        let classified = classify(&error);
        assert_eq!(classified.category, "state");
        assert_eq!(classified.code, "stale_state");
        assert!(classified.retryable);
        assert_eq!(classified.exit, 3);
    }

    #[test]
    fn diagnostics_are_truncated_on_utf8_boundaries() {
        let message = "ø".repeat(MAX_ERROR_MESSAGE_BYTES);
        let bounded = bounded_text(message);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.len() <= MAX_ERROR_MESSAGE_BYTES + 32);
        assert!(bounded.ends_with("[diagnostic truncated]"));
    }

    #[test]
    fn format_hint_stops_at_double_dash_and_handles_missing_values() {
        let positional = ["wheat", "add", "--", "--format=json"].map(std::ffi::OsString::from);
        let missing =
            ["wheat", "--format", "--format=json", "status"].map(std::ffi::OsString::from);
        assert_eq!(requested_format(&positional), OutputFormat::Human);
        assert_eq!(requested_format(&missing), OutputFormat::Json);
    }

    #[test]
    fn diagnostics_redact_url_credentials_and_queries() {
        let message = redact_urls(
            "git failed for https://user:token@example.invalid/repo?access_token=secret#fragment",
        );
        assert_eq!(
            message,
            "git failed for https://[redacted]@example.invalid/repo?[redacted]#fragment"
        );
        assert!(!message.contains("token"));
        assert!(!message.contains("secret"));
    }
}
