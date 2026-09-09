//! Focused Git remote observation and push support for Wheat projections.
//!
//! Remote URLs are passed directly to individual Git subprocesses. They are never
//! added to the projection repository's configuration.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use thiserror::Error;

/// Maximum stdout or stderr retained from any Git subprocess.
pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteOid(String);

impl RemoteOid {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn object_id(&self) -> Result<gix::ObjectId, PushError> {
        gix::ObjectId::from_hex(self.0.as_bytes()).map_err(|_| {
            PushError::InvalidRemoteOutput("remote object id is not supported by gix".into())
        })
    }

    fn parse(value: &str) -> Result<Self, PushError> {
        // Git's object-format transition permits SHA-1 and SHA-256 repositories.
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PushError::InvalidRemoteOutput(
                "ls-remote returned an invalid object id".into(),
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteState {
    Absent,
    Present(RemoteOid),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PushMode {
    /// Let `git push` enforce its normal fast-forward rules.
    #[default]
    Normal,
    /// Require that the destination remains absent.
    ForceWithLeaseAbsent,
    /// Permit a non-fast-forward update only when the destination still has this exact OID.
    ForceWithLease(RemoteOid),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefStatus {
    Updated,
    Forced,
    Created,
    Deleted,
    UpToDate,
    Rejected,
    Unknown(char),
}

impl RefStatus {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Updated | Self::Forced | Self::Created | Self::Deleted | Self::UpToDate
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefOutcome {
    pub source: String,
    pub destination: String,
    pub status: RefStatus,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedOutput {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl CapturedOutput {
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[derive(Debug)]
pub struct PushReport {
    pub exit_status: ExitStatus,
    pub refs: Vec<RefOutcome>,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

impl PushReport {
    pub fn accepted(&self) -> bool {
        self.exit_status.success()
            && !self.refs.is_empty()
            && self.refs.iter().all(|outcome| outcome.status.is_success())
    }
}

#[derive(Debug, Error)]
pub enum PushError {
    #[error("destination must be a valid full refs/heads ref: {0}")]
    InvalidDestination(String),
    #[error("source must be a valid full ref: {0}")]
    InvalidSource(String),
    #[error("projection repository is not a bare Git repository: {0}")]
    InvalidProjection(PathBuf),
    #[error("could not spawn git for {operation}: {source}")]
    Spawn {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("could not capture git output for {operation}: {message}")]
    Capture {
        operation: &'static str,
        message: String,
    },
    #[error("git {operation} failed with status {status:?}: {stderr}")]
    GitFailed {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("invalid remote output: {0}")]
    InvalidRemoteOutput(String),
}

/// Observe exactly one branch without adding or changing a configured remote.
pub fn observe_remote_ref(remote_url: &str, destination: &str) -> Result<RemoteState, PushError> {
    validate_destination(destination)?;
    let output = run_git(
        Command::new("git")
            .arg("ls-remote")
            .arg("--refs")
            .arg("--")
            .arg(remote_url)
            .arg(destination),
        "ls-remote",
    )?;
    require_success("ls-remote", &output)?;

    let text = String::from_utf8_lossy(&output.stdout.bytes);
    let mut observed = None;
    for line in text.lines() {
        let Some((oid, name)) = line.split_once('\t') else {
            return Err(PushError::InvalidRemoteOutput(
                "ls-remote returned a non-tab-separated line".into(),
            ));
        };
        if name != destination {
            return Err(PushError::InvalidRemoteOutput(format!(
                "ls-remote returned unexpected ref {name}"
            )));
        }
        if observed.is_some() {
            return Err(PushError::InvalidRemoteOutput(format!(
                "ls-remote returned {destination} more than once"
            )));
        }
        observed = Some(RemoteOid::parse(oid)?);
    }

    Ok(observed.map_or(RemoteState::Absent, RemoteState::Present))
}

/// Ensure an observed remote object is available in the bare projection repository.
///
/// The object is anchored at a Wheat-private ref. If the object is missing, the fetch
/// asks for the exact advertised OID rather than re-reading a potentially changed branch.
pub fn cache_observed_ref(
    projection: &Path,
    remote_url: &str,
    destination: &str,
    state: &RemoteState,
) -> Result<Option<String>, PushError> {
    validate_projection(projection)?;
    validate_destination(destination)?;
    let RemoteState::Present(oid) = state else {
        return Ok(None);
    };
    let private_ref = private_ref(destination);

    let exists = Command::new("git")
        .arg("-C")
        .arg(projection)
        .arg("cat-file")
        .arg("-e")
        .arg(oid.as_str())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| PushError::Spawn {
            operation: "cat-file",
            source,
        })?
        .success();

    let output = if exists {
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(projection)
                .arg("update-ref")
                .arg(&private_ref)
                .arg(oid.as_str()),
            "update-ref",
        )?
    } else {
        let exact = format!("+{}:{private_ref}", oid.as_str());
        let mut output = run_git(
            Command::new("git")
                .arg("-C")
                .arg(projection)
                .arg("fetch")
                .arg("--no-tags")
                .arg("--")
                .arg(remote_url)
                .arg(exact),
            "fetch",
        )?;
        if !output.status.success() {
            let advertised = format!("+{destination}:{private_ref}");
            output = run_git(
                Command::new("git")
                    .arg("-C")
                    .arg(projection)
                    .arg("fetch")
                    .arg("--no-tags")
                    .arg("--")
                    .arg(remote_url)
                    .arg(advertised),
                "fetch",
            )?;
        }
        output
    };
    require_success(if exists { "update-ref" } else { "fetch" }, &output)?;
    let cached = run_git(
        Command::new("git")
            .arg("-C")
            .arg(projection)
            .arg("rev-parse")
            .arg("--verify")
            .arg(&private_ref),
        "rev-parse",
    )?;
    require_success("rev-parse", &cached)?;
    if cached.stdout.to_string_lossy().trim() != oid.as_str() {
        return Err(PushError::InvalidRemoteOutput(
            "remote ref changed while its observed object was being cached".into(),
        ));
    }
    Ok(Some(private_ref))
}

/// Return whether `ancestor` is reachable from `descendant` in the projection repository.
pub fn is_ancestor(
    projection: &Path,
    ancestor: &RemoteOid,
    descendant: gix::ObjectId,
) -> Result<bool, PushError> {
    validate_projection(projection)?;
    let status = Command::new("git")
        .arg("-C")
        .arg(projection)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor.as_str())
        .arg(descendant.to_string())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| PushError::Spawn {
            operation: "merge-base",
            source,
        })?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(PushError::GitFailed {
            operation: "merge-base",
            status: status.code(),
            stderr: "could not determine projected ancestry".into(),
        }),
    }
}

/// Push one full local ref to one full branch ref and return Git's per-ref outcome.
///
/// A failed/rejected push is represented by a [`PushReport`], not a transport error,
/// because porcelain output contains useful per-ref rejection information.
pub fn push_ref(
    projection: &Path,
    remote_url: &str,
    source: &str,
    destination: &str,
    mode: PushMode,
) -> Result<PushReport, PushError> {
    validate_projection(projection)?;
    validate_source(source)?;
    validate_destination(destination)?;

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(projection)
        .arg("push")
        .arg("--porcelain");
    match mode {
        PushMode::Normal => {}
        PushMode::ForceWithLeaseAbsent => {
            command.arg(format!("--force-with-lease={destination}:"));
        }
        PushMode::ForceWithLease(observed) => {
            command.arg(format!(
                "--force-with-lease={destination}:{}",
                observed.as_str()
            ));
        }
    }
    command
        .arg("--")
        .arg(remote_url)
        .arg(format!("{source}:{destination}"));

    let output = run_git(&mut command, "push")?;
    if output.stdout.truncated || output.stderr.truncated {
        return Err(PushError::InvalidRemoteOutput(format!(
            "git push output exceeded {MAX_CAPTURED_OUTPUT_BYTES} bytes per stream"
        )));
    }
    let refs = parse_porcelain(&output.stdout.bytes);
    Ok(PushReport {
        exit_status: output.status,
        refs,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn validate_projection(path: &Path) -> Result<(), PushError> {
    if path.is_dir() && path.join("HEAD").is_file() && path.join("objects").is_dir() {
        Ok(())
    } else {
        Err(PushError::InvalidProjection(path.to_path_buf()))
    }
}

fn validate_destination(reference: &str) -> Result<(), PushError> {
    if !reference.starts_with("refs/heads/") || reference == "refs/heads/" {
        return Err(PushError::InvalidDestination(reference.into()));
    }
    validate_ref_with_git(reference, true)
}

fn validate_source(reference: &str) -> Result<(), PushError> {
    if !reference.starts_with("refs/") {
        return Err(PushError::InvalidSource(reference.into()));
    }
    validate_ref_with_git(reference, false)
}

fn validate_ref_with_git(reference: &str, destination: bool) -> Result<(), PushError> {
    let valid = Command::new("git")
        .arg("check-ref-format")
        .arg(reference)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| PushError::Spawn {
            operation: "check-ref-format",
            source,
        })?
        .success();
    if valid {
        Ok(())
    } else if destination {
        Err(PushError::InvalidDestination(reference.into()))
    } else {
        Err(PushError::InvalidSource(reference.into()))
    }
}

fn private_ref(destination: &str) -> String {
    format!(
        "refs/wheat/remote-cache/{}",
        destination
            .strip_prefix("refs/")
            .expect("validated full ref")
    )
}

fn parse_porcelain(stdout: &[u8]) -> Vec<RefOutcome> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let flag = fields.next()?;
            let refspec = fields.next()?;
            let summary = fields.next().unwrap_or_default();
            let (source, destination) = refspec.split_once(':')?;
            let marker = flag.chars().next()?;
            let status = match marker {
                ' ' => RefStatus::Updated,
                '+' => RefStatus::Forced,
                '*' => RefStatus::Created,
                '-' => RefStatus::Deleted,
                '=' => RefStatus::UpToDate,
                '!' => RefStatus::Rejected,
                other => RefStatus::Unknown(other),
            };
            Some(RefOutcome {
                source: source.into(),
                destination: destination.into(),
                status,
                summary: summary.into(),
            })
        })
        .collect()
}

struct CommandOutput {
    status: ExitStatus,
    stdout: CapturedOutput,
    stderr: CapturedOutput,
}

fn run_git(command: &mut Command, operation: &'static str) -> Result<CommandOutput, PushError> {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|source| PushError::Spawn { operation, source })?;
    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    let stdout_thread = thread::spawn(move || capture_bounded(stdout));
    let stderr_thread = thread::spawn(move || capture_bounded(stderr));
    let status = child
        .wait()
        .map_err(|source| PushError::Spawn { operation, source })?;
    let stdout = join_capture(stdout_thread, operation)?;
    let stderr = join_capture(stderr_thread, operation)?;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn capture_bounded(mut reader: impl Read) -> io::Result<CapturedOutput> {
    let mut retained = Vec::with_capacity(8192);
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep != read;
    }
    Ok(CapturedOutput {
        bytes: retained,
        truncated,
    })
}

fn join_capture(
    handle: thread::JoinHandle<io::Result<CapturedOutput>>,
    operation: &'static str,
) -> Result<CapturedOutput, PushError> {
    handle
        .join()
        .map_err(|_| PushError::Capture {
            operation,
            message: "output reader thread panicked".into(),
        })?
        .map_err(|error| PushError::Capture {
            operation,
            message: error.to_string(),
        })
}

fn require_success(operation: &'static str, output: &CommandOutput) -> Result<(), PushError> {
    if output.stdout.truncated || output.stderr.truncated {
        return Err(PushError::InvalidRemoteOutput(format!(
            "git {operation} output exceeded {MAX_CAPTURED_OUTPUT_BYTES} bytes per stream"
        )));
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(PushError::GitFailed {
            operation,
            status: output.status.code(),
            stderr: output.stderr.to_string_lossy(),
        })
    }
}
