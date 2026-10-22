//! What a service returns to its caller: a result document with its human
//! rendering, or a classified failure.

use std::fmt;

use anyhow::Error;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Serializes an identity as the hex string it displays as, for JSON documents.
pub fn display_id<T: fmt::Display, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

pub fn display_optional_id<T: fmt::Display, S: serde::Serializer>(
    value: &Option<T>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.collect_str(value),
        None => serializer.serialize_none(),
    }
}

pub fn display_ids<T: fmt::Display, S: serde::Serializer>(
    values: &[T],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(Debug, Error)]
pub enum CliFailure {
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
    /// Adding would move edits out of later changes, which needs confirmation or `--force`.
    #[error("{0}")]
    EditsWouldMove(String),
    /// Materializing would write inside a `.git` directory, which needs confirmation or
    /// `--allow-dot-git`.
    #[error("{0}")]
    DotGitPaths(String),
}

impl CliFailure {
    pub fn invalid(message: impl Into<String>) -> Error {
        Self::InvalidInput(message.into()).into()
    }

    pub fn stale(message: impl Into<String>) -> Error {
        Self::StaleState(message.into()).into()
    }

    pub fn conflict(message: impl Into<String>) -> Error {
        Self::Conflict(message.into()).into()
    }

    pub fn limit(message: impl Into<String>) -> Error {
        Self::LimitExceeded(message.into()).into()
    }

    pub fn corrupt(message: impl Into<String>) -> Error {
        Self::CorruptState(message.into()).into()
    }

    pub fn transport(message: impl Into<String>) -> Error {
        Self::TransportFailure(message.into()).into()
    }

    pub fn would_move(message: impl Into<String>) -> Error {
        Self::EditsWouldMove(message.into()).into()
    }

    pub fn dot_git(message: impl Into<String>) -> Error {
        Self::DotGitPaths(message.into()).into()
    }
}

pub struct Success {
    pub command: &'static str,
    pub result: Value,
    pub human: Vec<u8>,
}

impl Success {
    pub fn new(
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

    pub fn raw(
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
impl fmt::Display for Success {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.command)
    }
}
