//! Wall time and peak memory, for child processes and for phases inside this one.
//!
//! A child is reaped with `wait4`, whose `rusage` reports the child's peak resident
//! set size. A child's own waited-for children are folded into it by the kernel, so
//! the peak of a `wheat` command that runs `git` covers `git` too. This is what GNU
//! `time` reports as `%M`, without needing `/usr/bin/time`.
//!
//! The kernel counts a child's peak from the moment it is forked: on `exec` it folds
//! the high-water mark of the memory it was forked with, which is its parent's, into
//! the peak `wait4` reports. A `true` started by a parent of 400 MiB reports 400 MiB.
//! So once a process has grown (a case with a server in it), it measures its children
//! through a launcher instead ([`measure_through`]): this binary started afresh, which
//! stays small, starts the child, reaps it, and reports what `wait4` said ([`time`]).
//!
//! Inside one process, [`PhasePeak`] reads the `VmHWM` high-water mark and resets it
//! between phases through `/proc/self/clear_refs`, so each phase gets its own peak.
//! The reset also lowers the peak a parent later reads from `wait4` (the kernel keeps
//! one high-water mark for both), so whoever reports a whole-process peak for a
//! process that used [`PhasePeak`] must also take the phase peaks into account.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::OnceLock,
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// Wheat's own structured/human output ceiling is 8 MiB. The small margin accommodates
/// envelope/error text while keeping every captured stream bounded.
pub const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub wall_seconds: f64,
    pub peak_rss_bytes: f64,
}

/// A finished child: its exit status, bounded output, and what it cost.
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timing: Timing,
}

impl Captured {
    /// Fails with both output streams when the child did not succeed.
    pub fn ensure_success(&self, what: &str) -> Result<()> {
        ensure!(
            self.status.success(),
            "{what} failed with {}\nstdout:\n{}\nstderr:\n{}",
            self.status,
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        );
        Ok(())
    }
}

/// The launcher every child is measured through, once [`measure_through`] set it.
static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();

/// Measures every child [`run`] starts from now on through `launcher`, a binary that
/// runs [`time`] for the arguments `time --report <file> -- <program> <arguments>`.
/// `wheat-bench` passes itself. Without it, children are measured directly, and their
/// peak is at least this process's size when they started.
pub fn measure_through(launcher: PathBuf) {
    // A second call names the same binary.
    let _ = LAUNCHER.set(launcher);
}

/// What the launcher learned about the child it ran.
#[derive(Serialize, Deserialize)]
struct LaunchReport {
    /// The raw wait status, as `waitpid` returns it.
    wait_status: i32,
    wall_seconds: f64,
    peak_rss_bytes: f64,
}

/// The launcher side of [`measure_through`]: runs `program` with `arguments`, which
/// inherit this process's directory, environment and standard streams, and writes its
/// wait status, wall time and peak to `report`.
pub fn time(report: &Path, program: &OsStr, arguments: &[OsString]) -> Result<()> {
    let started = Instant::now();
    let child = Command::new(program)
        .args(arguments)
        .spawn()
        .with_context(|| format!("start {program:?}"))?;
    let (status, peak_rss_bytes) = wait_with_rusage(child.id())?;
    let wall_seconds = started.elapsed().as_secs_f64();
    drop(child);
    let launched = LaunchReport {
        wait_status: status.into_raw(),
        wall_seconds,
        peak_rss_bytes,
    };
    std::fs::write(report, serde_json::to_vec(&launched)?)
        .with_context(|| format!("write {}", report.display()))
}

/// `command` as the launcher's arguments, with its directory and environment.
fn through_launcher(launcher: &Path, report: &Path, command: &Command) -> Command {
    let mut launched = Command::new(launcher);
    launched
        .arg("time")
        .arg("--report")
        .arg(report)
        .arg("--")
        .arg(command.get_program())
        .args(command.get_args());
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => launched.env(name, value),
            None => launched.env_remove(name),
        };
    }
    if let Some(directory) = command.get_current_dir() {
        launched.current_dir(directory);
    }
    launched
}

/// Runs `command` to completion and measures it.
///
/// Output goes to anonymous files in `scratch` rather than pipes, so the child never
/// blocks on a full pipe, and only the first [`MAX_CAPTURE_BYTES`] of each stream are
/// read back.
pub fn run(command: &mut Command, scratch: &Path) -> Result<Captured> {
    run_capturing(command, scratch, true)
}

/// [`run`], but the child writes its stderr straight to ours (so its progress shows
/// while it runs) and [`Captured::stderr`] is empty.
pub fn run_showing_stderr(command: &mut Command, scratch: &Path) -> Result<Captured> {
    run_capturing(command, scratch, false)
}

fn run_capturing(command: &mut Command, scratch: &Path, capture_stderr: bool) -> Result<Captured> {
    let mut launched = match LAUNCHER.get() {
        Some(launcher) => {
            let report = tempfile::NamedTempFile::new_in(scratch)?;
            let launched = through_launcher(launcher, report.path(), command);
            Some((report, launched))
        }
        None => None,
    };
    let spawned = match &mut launched {
        Some((_, launched)) => launched,
        None => &mut *command,
    };
    let stdout_file = tempfile::tempfile_in(scratch)?;
    spawned.stdout(Stdio::from(stdout_file.try_clone()?));
    let stderr_file = if capture_stderr {
        let file = tempfile::tempfile_in(scratch)?;
        spawned.stderr(Stdio::from(file.try_clone()?));
        Some(file)
    } else {
        spawned.stderr(Stdio::inherit());
        None
    };
    let started = Instant::now();
    let child = spawned
        .spawn()
        .with_context(|| format!("start {:?}", spawned.get_program()))?;
    let (status, peak_rss_bytes) = wait_with_rusage(child.id())?;
    let wall_seconds = started.elapsed().as_secs_f64();
    // `wait4` reaped the child; `Child`'s drop neither waits nor kills.
    drop(child);
    let stdout = read_bounded(stdout_file, MAX_CAPTURE_BYTES).context("read child stdout")?;
    let stderr = match stderr_file {
        Some(file) => read_bounded(file, MAX_CAPTURE_BYTES).context("read child stderr")?,
        None => Vec::new(),
    };
    let (status, timing) = match launched {
        None => (
            status,
            Timing {
                wall_seconds,
                peak_rss_bytes,
            },
        ),
        Some((report, _)) => {
            let launched: LaunchReport = std::fs::read(report.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .with_context(|| {
                    format!(
                        "the launcher did not run {:?}: it exited with {status}\n{}",
                        command.get_program(),
                        String::from_utf8_lossy(&stderr)
                    )
                })?;
            (
                ExitStatus::from_raw(launched.wait_status),
                Timing {
                    wall_seconds: launched.wall_seconds,
                    peak_rss_bytes: launched.peak_rss_bytes,
                },
            )
        }
    };
    Ok(Captured {
        status,
        stdout,
        stderr,
        timing,
    })
}

fn wait_with_rusage(pid: u32) -> Result<(ExitStatus, f64)> {
    let pid = libc::pid_t::try_from(pid).context("child pid out of range")?;
    let mut status = 0;
    // SAFETY: an all-zero `rusage` is a valid value; `wait4` overwrites it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `status` and `usage` are valid, exclusive out-pointers for this call.
        let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        if reaped == pid {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("wait for child");
        }
    }
    // Linux reports `ru_maxrss` in KiB.
    Ok((
        ExitStatus::from_raw(status),
        usage.ru_maxrss as f64 * 1024.0,
    ))
}

/// Reads at most `limit` bytes from the start of `file`, failing if there are more.
pub fn read_bounded(mut file: File, limit: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(limit.min(8192));
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "command output exceeded {limit} byte limit"
    );
    Ok(bytes)
}

/// The peak of several sequential measurements.
pub fn peak_rss_bytes<'a>(timings: impl IntoIterator<Item = &'a Timing>) -> f64 {
    timings
        .into_iter()
        .map(|timing| timing.peak_rss_bytes)
        .fold(0.0, f64::max)
}

/// The middle value; for an even count, the upper of the two middle values.
pub fn median(mut values: Vec<f64>) -> f64 {
    assert!(!values.is_empty(), "median of no samples");
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Per-phase peak resident set size of this process.
///
/// Resetting the mark also resets what `wait4` will report for this process; see the
/// module docs.
pub struct PhasePeak(());

impl PhasePeak {
    /// Starts measuring: the first phase's peak excludes anything before this call.
    pub fn start() -> Result<Self> {
        reset_high_water_mark()?;
        Ok(Self(()))
    }

    /// The peak since the previous call (or [`PhasePeak::start`]); resets the mark.
    pub fn take(&mut self) -> Result<f64> {
        let peak = high_water_mark_bytes()?;
        reset_high_water_mark()?;
        Ok(peak)
    }
}

fn high_water_mark_bytes() -> Result<f64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let kib: f64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .context("/proc/self/status has no VmHWM")?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()?;
    Ok(kib * 1024.0)
}

/// Linux `clear_refs` value 5 resets the peak RSS to the current RSS.
fn reset_high_water_mark() -> Result<()> {
    std::fs::write("/proc/self/clear_refs", "5").context("reset peak RSS")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn a_child_reports_its_wall_time_output_and_peak_memory() {
        let scratch = tempfile::tempdir().unwrap();
        // `dd` fills one 64 MiB block buffer, so the child's peak is at least that.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "sleep 0.2; dd if=/dev/zero of=/dev/null bs=64M count=1 2>/dev/null; echo done",
        ]);
        let captured = run(&mut command, scratch.path()).unwrap();
        captured.ensure_success("sh").unwrap();
        assert_eq!(captured.stdout, b"done\n");
        assert!(captured.timing.wall_seconds >= 0.2, "{:?}", captured.timing);
        assert!(
            captured.timing.peak_rss_bytes >= 64.0 * 1024.0 * 1024.0,
            "{:?}",
            captured.timing
        );
    }

    #[test]
    fn a_failing_child_is_reported_with_its_output() {
        let scratch = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "echo oops >&2; exit 3"]);
        let captured = run(&mut command, scratch.path()).unwrap();
        assert_eq!(captured.status.code(), Some(3));
        let error = captured.ensure_success("sh").unwrap_err().to_string();
        assert!(error.contains("oops"), "{error}");
    }

    #[test]
    fn bounded_file_read_accepts_limit_and_rejects_excess() {
        let mut exact = tempfile::tempfile().unwrap();
        exact.write_all(b"1234").unwrap();
        assert_eq!(read_bounded(exact, 4).unwrap(), b"1234");

        let mut excess = tempfile::tempfile().unwrap();
        excess.write_all(b"12345").unwrap();
        assert!(read_bounded(excess, 4).is_err());
    }

    #[test]
    fn median_and_peak() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0]), 4.0);
        let timings = [1024.0, 4096.0].map(|peak_rss_bytes| Timing {
            wall_seconds: 1.0,
            peak_rss_bytes,
        });
        assert_eq!(peak_rss_bytes(&timings), 4096.0);
        assert_eq!(peak_rss_bytes(&[]), 0.0);
    }

    #[test]
    fn phase_peaks_are_separate() {
        let mut peak = PhasePeak::start().unwrap();
        let big = vec![1_u8; 96 * 1024 * 1024];
        std::hint::black_box(&big);
        drop(big);
        let first = peak.take().unwrap();
        let second = peak.take().unwrap();
        assert!(first >= 96.0 * 1024.0 * 1024.0, "{first}");
        assert!(second < first, "{second} after {first}");
    }
}
