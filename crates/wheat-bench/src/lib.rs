//! Wheat's benchmark profiles.
//!
//! - `wheat-quick` ([`quick`]): a few minutes of focused scale cases.
//! - `wheat-gothab-workflow` ([`corpus`]): the whole CLI workflow against a large Git
//!   corpus (about 55 minutes on the Gothab repository).
//!
//! Each run writes a Gothab V1 profile report ([`report`]) for the workflow's
//! `actions/profile` step to upload, and local runs also save a results file under
//! `bench/results/` ([`history`]) so runs can be compared over time.
#![cfg(target_os = "linux")]

pub mod corpus;
pub mod history;
pub mod host;
pub mod measure;
pub mod quick;
pub mod report;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};

use crate::{
    history::Results,
    report::{ContractOutcome, ProfileReportV1},
};

/// The `wheat` binary to measure: `WHEAT_BENCH_WHEAT`, or the `wheat` cargo built into
/// the same target directory as this binary (`target/release/wheat-bench` →
/// `target/release/wheat`).
pub fn wheat_binary() -> Result<PathBuf> {
    let wheat = match std::env::var_os("WHEAT_BENCH_WHEAT") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("wheat-bench is not inside a directory")?
            .join("wheat"),
    };
    ensure!(
        wheat.is_file(),
        "no wheat binary at {}; build it with `cargo build --release --bin wheat` \
         or set WHEAT_BENCH_WHEAT",
        wheat.display()
    );
    Ok(wheat)
}

/// Where benchmarks create their temporary directories: `WHEAT_BENCH_SCRATCH_ROOT`,
/// or the system temporary directory.
pub fn scratch_root() -> Result<PathBuf> {
    let root =
        std::env::var_os("WHEAT_BENCH_SCRATCH_ROOT").map_or_else(std::env::temp_dir, PathBuf::from);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    Ok(root)
}

/// Gives a child process Git's built-in defaults rather than the developer's own
/// configuration, so results don't depend on who runs them.
pub fn isolate(command: &mut Command) -> &mut Command {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
}

/// Where a finished report goes.
pub struct Destination {
    /// The canonical report JSON, for `actions/profile`.
    pub report: PathBuf,
    /// The `bench/results` directory to save a results file in, or `None`.
    pub history: Option<PathBuf>,
}

impl Destination {
    /// `report`, plus `<checkout>/bench/results` unless `no_save`.
    pub fn new(report: PathBuf, no_save: bool) -> Result<Self> {
        let history = if no_save {
            None
        } else {
            let checkout = host::Checkout::find(&std::env::current_dir()?)
                .context("saving results needs a Git checkout; pass --no-save to skip")?;
            Some(checkout.root.join("bench/results"))
        };
        Ok(Self { report, history })
    }
}

/// Writes the report and the results file, then evaluates the contract.
///
/// Both are written before evaluating: a failed contract is still a valid profile
/// result that the workflow must upload.
pub fn publish(report: &ProfileReportV1, destination: &Destination) -> Result<()> {
    fs::write(&destination.report, report.canonical_json()?)
        .with_context(|| format!("write {}", destination.report.display()))?;
    eprintln!("wrote {}", destination.report.display());
    if let Some(history) = &destination.history {
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let saved = Results::from_report(report, &stamp).save(history)?;
        eprintln!("saved {}", saved.display());
    }
    match report.evaluate()? {
        ContractOutcome::Passed => Ok(()),
        ContractOutcome::Failed { violations } => {
            let mut message = String::from("performance contract failed:");
            for violation in violations {
                message.push_str(&format!(
                    "\n  {} = {} {}, limit {:?} {}",
                    violation.measurement,
                    violation.actual,
                    violation.unit,
                    violation.comparison,
                    violation.threshold
                ));
            }
            anyhow::bail!(message)
        }
    }
}

/// A path as UTF-8, for passing to `wheat` as an argument.
pub fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::report::{Contract, ContractRule, Measurement, Workload};

    fn report(wall: f64) -> ProfileReportV1 {
        ProfileReportV1 {
            schema_version: 1,
            profile: "wheat-quick".into(),
            workload: Workload {
                name: "wheat-quick".into(),
                parameters: BTreeMap::from([("rev".into(), json!("abc"))]),
            },
            contract: Contract {
                rules: vec![ContractRule::at_most("run.total_time", 10.0, "s")],
            },
            measurements: vec![Measurement::new("run.total_time", wall, "s")],
        }
    }

    #[test]
    fn a_failed_contract_still_writes_the_report_and_results() {
        let directory = tempfile::tempdir().unwrap();
        let destination = Destination {
            report: directory.path().join("profile.json"),
            history: Some(directory.path().join("results")),
        };
        let error = publish(&report(11.0), &destination).unwrap_err();
        assert!(
            error.to_string().contains("run.total_time = 11 s"),
            "{error}"
        );
        let written: ProfileReportV1 =
            serde_json::from_slice(&fs::read(&destination.report).unwrap()).unwrap();
        assert_eq!(written, report(11.0));
        let latest =
            Results::read(&directory.path().join("results/wheat-quick/latest.tsv")).unwrap();
        assert_eq!(latest.measurements, report(11.0).measurements);

        publish(&report(9.0), &destination).unwrap();
    }
}
