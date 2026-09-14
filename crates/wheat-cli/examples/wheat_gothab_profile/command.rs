use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

// Wheat's own structured/human output ceiling is 8 MiB. The small margin
// accommodates envelope/error text while keeping every captured stream bounded.
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;
const MAX_TIMING_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Timing {
    pub(crate) wall_seconds: f64,
    pub(crate) peak_rss_bytes: f64,
}

pub(crate) struct BoundedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) struct ProfileRun {
    wheat: PathBuf,
    workspace: PathBuf,
    store_home: PathBuf,
    config_home: PathBuf,
    pub(crate) timings: Vec<(String, Timing)>,
}

impl ProfileRun {
    pub(crate) fn new(
        wheat: PathBuf,
        workspace: PathBuf,
        store_home: PathBuf,
        config_home: PathBuf,
    ) -> Self {
        Self {
            wheat,
            workspace,
            store_home,
            config_home,
            timings: Vec::new(),
        }
    }

    pub(crate) fn store_home(&self) -> &Path {
        &self.store_home
    }

    /// Runs each measured phase as an independent Wheat subprocess. GNU time
    /// wraps that subprocess rather than timing in-process library calls.
    pub(crate) fn wheat(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
    ) -> Result<BoundedOutput> {
        self.run_wheat(name, current_dir, arguments, true)
    }

    pub(crate) fn wheat_ok(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
    ) -> Result<()> {
        self.run_wheat(name, current_dir, arguments, false)?;
        Ok(())
    }

    fn run_wheat(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
        retain_output: bool,
    ) -> Result<BoundedOutput> {
        let scratch = self.workspace.parent().context("workspace has no parent")?;
        let timing_file = tempfile::NamedTempFile::new_in(scratch)?;
        let stdout_file = tempfile::tempfile_in(scratch)?;
        let stderr_file = tempfile::tempfile_in(scratch)?;
        eprintln!("profile phase {name}: starting");
        let mut command = Command::new("/usr/bin/time");
        command
            .args(["-f", "%e %M", "-o"])
            .arg(timing_file.path())
            .arg(&self.wheat)
            .args(arguments)
            .current_dir(current_dir)
            .env("WHEAT_STORE_HOME", &self.store_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::from(stdout_file.try_clone()?));
        command.stderr(Stdio::from(stderr_file.try_clone()?));
        let status = command
            .status()
            .with_context(|| format!("run timed Wheat phase {name}"))?;
        let stdout = read_bounded(stdout_file, MAX_CAPTURE_BYTES)
            .with_context(|| format!("read {name} stdout"))?;
        let stderr = read_bounded(stderr_file, MAX_CAPTURE_BYTES)
            .with_context(|| format!("read {name} stderr"))?;
        ensure!(
            status.success(),
            "Wheat phase {name} failed with {status}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        let timing_bytes = read_bounded(timing_file.reopen()?, MAX_TIMING_BYTES)
            .with_context(|| format!("read {name} GNU time output"))?;
        let timing_text = std::str::from_utf8(&timing_bytes)
            .with_context(|| format!("decode {name} GNU time output"))?;
        let timing = parse_time_output(timing_text)?;
        eprintln!(
            "profile phase {name}: completed in {:.2}s, peak RSS {:.1} MiB",
            timing.wall_seconds,
            timing.peak_rss_bytes / (1024.0 * 1024.0)
        );
        self.timings.push((name.to_owned(), timing));
        Ok(if retain_output {
            BoundedOutput { stdout, stderr }
        } else {
            BoundedOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
            }
        })
    }

    pub(crate) fn json(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
    ) -> Result<Value> {
        let mut args = Vec::with_capacity(arguments.len() + 1);
        args.push("--format=json");
        args.extend_from_slice(arguments);
        let output = self.wheat(name, current_dir, &args)?;
        ensure!(output.stderr.is_empty(), "{name} JSON command wrote stderr");
        let value: Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("parse {name} JSON output"))?;
        ensure!(
            value["ok"] == true,
            "{name} returned a non-success document"
        );
        Ok(value)
    }
}

fn read_bounded(mut file: File, limit: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "command output exceeded {limit} byte limit"
    );
    Ok(bytes)
}

pub(crate) fn peak_process_rss_bytes(timings: &[(String, Timing)]) -> f64 {
    // Phases run sequentially as separate subprocesses. GNU time's max RSS is
    // kernel-accounted for each complete child process tree, so their maximum is
    // the defensible workload peak without a racy userspace /proc tree scan.
    timings
        .iter()
        .map(|(_, timing)| timing.peak_rss_bytes)
        .fold(0.0, f64::max)
}

fn parse_time_output(value: &str) -> Result<Timing> {
    let mut fields = value.split_whitespace();
    let wall_seconds = fields.next().context("missing elapsed time")?.parse()?;
    let peak_kib: f64 = fields.next().context("missing max RSS")?.parse()?;
    ensure!(
        fields.next().is_none(),
        "unexpected GNU time output: {value:?}"
    );
    ensure!(
        wall_seconds >= 0.0 && peak_kib >= 0.0,
        "negative GNU time value"
    );
    Ok(Timing {
        wall_seconds,
        peak_rss_bytes: peak_kib * 1024.0,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn parses_gnu_time_wall_and_rss() {
        assert_eq!(
            parse_time_output("12.34 5678\n").unwrap(),
            Timing {
                wall_seconds: 12.34,
                peak_rss_bytes: 5678.0 * 1024.0
            }
        );
        assert!(parse_time_output("12.34").is_err());
        assert!(parse_time_output("-1 3").is_err());
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
    fn complete_run_peak_is_maximum_sequential_phase_peak() {
        let timings = vec![
            (
                "first".into(),
                Timing {
                    wall_seconds: 1.0,
                    peak_rss_bytes: 1024.0,
                },
            ),
            (
                "second".into(),
                Timing {
                    wall_seconds: 1.0,
                    peak_rss_bytes: 4096.0,
                },
            ),
        ];
        assert_eq!(peak_process_rss_bytes(&timings), 4096.0);
        assert_eq!(peak_process_rss_bytes(&[]), 0.0);
    }
}
