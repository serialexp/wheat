//! Runs each measured corpus phase as its own `wheat` process.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::measure::{self, Timing};

pub(crate) struct BoundedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) struct Session {
    wheat: PathBuf,
    scratch: PathBuf,
    store_home: PathBuf,
    config_home: PathBuf,
    pub(crate) timings: Vec<(String, Timing)>,
}

impl Session {
    pub(crate) fn new(wheat: PathBuf, scratch: PathBuf) -> Self {
        Self {
            wheat,
            store_home: scratch.join("stores"),
            config_home: scratch.join("config"),
            scratch,
            timings: Vec::new(),
        }
    }

    pub(crate) fn store_home(&self) -> &Path {
        &self.store_home
    }

    /// Runs one measured phase and keeps its output.
    pub(crate) fn wheat(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
    ) -> Result<BoundedOutput> {
        eprintln!("profile phase {name}: starting");
        let mut command = crate::wheat_command(&self.wheat);
        command
            .args(arguments)
            .current_dir(current_dir)
            .env("WHEAT_STORE_HOME", &self.store_home)
            .env("XDG_CONFIG_HOME", &self.config_home);
        let captured = measure::run(&mut command, &self.scratch)
            .with_context(|| format!("run Wheat phase {name}"))?;
        captured.ensure_success(&format!("Wheat phase {name}"))?;
        eprintln!(
            "profile phase {name}: completed in {:.2}s, peak RSS {:.1} MiB",
            captured.timing.wall_seconds,
            captured.timing.peak_rss_bytes / (1024.0 * 1024.0)
        );
        self.timings.push((name.to_owned(), captured.timing));
        Ok(BoundedOutput {
            stdout: captured.stdout,
            stderr: captured.stderr,
        })
    }

    pub(crate) fn wheat_ok(
        &mut self,
        name: &str,
        current_dir: &Path,
        arguments: &[&str],
    ) -> Result<()> {
        self.wheat(name, current_dir, arguments).map(drop)
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
