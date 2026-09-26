//! The `wheat-quick` profile: focused scale cases that finish in minutes.
//!
//! Every case runs in its own child process (`wheat-bench case <name>`), so its
//! whole-case peak memory is its own and a crash or failed check in one case doesn't
//! stop the others. The child prints its [`CaseOutput`] as JSON on stdout, with
//! measurements named `<case>.<step>.<quantity>`; the parent adds each case's
//! `<case>.total.wall` and `<case>.total.peak_rss` and builds one report. A case that
//! fails is left out and counted in `suite.case_failures`.
//!
//! Cases carry contract rules for the full sizes. At [`Scale::Smoke`] the sizes are
//! too small for those budgets to mean anything, so only `suite.case_failures` is
//! checked.

mod add_scale;
mod checkpoint_stack;
mod content_repack_plan;
mod deep_stack;
mod git_export_notes;
mod history;
mod packed_append;
mod sharded_manifest;
mod transition_validate;
mod workspace;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    host::{self, Checkout},
    measure::{self, Timing},
    report::{Contract, ContractRule, Measurement, ProfileReportV1, Workload},
};

pub const PROFILE: &str = "wheat-quick";

/// Every case, in the order they run.
pub const CASES: &[&str] = &[
    deep_stack::NAME,
    add_scale::NAME,
    transition_validate::NAME,
    packed_append::NAME,
    content_repack_plan::NAME,
    sharded_manifest::NAME,
    git_export_notes::NAME,
    checkpoint_stack::NAME,
];

/// How big the cases are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scale {
    /// The sizes the budgets are set for.
    Full,
    /// Tiny sizes that only check every case still runs; budgets are not checked.
    Smoke,
}

impl Scale {
    fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Smoke => "smoke",
        }
    }

    /// `full` at full scale, `smoke` otherwise.
    fn pick<T>(self, full: T, smoke: T) -> T {
        match self {
            Self::Full => full,
            Self::Smoke => smoke,
        }
    }
}

/// What one case reports back to the parent.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOutput {
    pub measurements: Vec<Measurement>,
    pub rules: Vec<ContractRule>,
    /// The case's sizes, recorded as the workload parameter named after the case.
    pub parameters: BTreeMap<String, Value>,
}

/// Builds a [`CaseOutput`] whose measurement names start with the case name.
pub(crate) struct Recorder {
    case: &'static str,
    output: CaseOutput,
}

impl Recorder {
    pub(crate) fn new(case: &'static str) -> Self {
        Self {
            case,
            output: CaseOutput::default(),
        }
    }

    fn name(&self, name: &str) -> String {
        format!("{}.{name}", self.case)
    }

    pub(crate) fn parameter(&mut self, key: &str, value: impl Serialize) {
        self.output.parameters.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("parameters serialize"),
        );
    }

    pub(crate) fn value(&mut self, name: &str, value: f64, unit: &str) {
        let name = self.name(name);
        self.output
            .measurements
            .push(Measurement::new(name, value, unit));
    }

    /// `<step>.wall` in seconds.
    pub(crate) fn wall(&mut self, step: &str, seconds: f64) {
        self.value(&format!("{step}.wall"), seconds, "s");
    }

    /// `<step>.peak_rss` in bytes.
    pub(crate) fn peak_rss(&mut self, step: &str, bytes: f64) {
        self.value(&format!("{step}.peak_rss"), bytes, "bytes");
    }

    /// `<step>.wall` and `<step>.peak_rss` of one measured process.
    pub(crate) fn timing(&mut self, step: &str, timing: Timing) {
        self.wall(step, timing.wall_seconds);
        self.peak_rss(step, timing.peak_rss_bytes);
    }

    /// The median wall time and the highest peak RSS of several runs of one step.
    pub(crate) fn runs(&mut self, step: &str, runs: &[Timing]) -> f64 {
        let wall = measure::median(runs.iter().map(|run| run.wall_seconds).collect());
        self.wall(step, wall);
        self.peak_rss(step, measure::peak_rss_bytes(runs));
        wall
    }

    pub(crate) fn at_most(&mut self, name: &str, value: f64, unit: &str) {
        let name = self.name(name);
        self.output
            .rules
            .push(ContractRule::at_most(name, value, unit));
    }

    pub(crate) fn finish(self) -> CaseOutput {
        self.output
    }
}

/// What a case needs from its environment.
pub(crate) struct CaseContext {
    pub(crate) scale: Scale,
    pub(crate) wheat: PathBuf,
    pub(crate) scratch: PathBuf,
}

/// Runs one case in this process. This is what `wheat-bench case <name>` does.
pub fn run_case(
    name: &str,
    scale: Scale,
    wheat: PathBuf,
    scratch_root: &Path,
) -> Result<CaseOutput> {
    let scratch = tempfile::Builder::new()
        .prefix(&format!("wheat-bench-{name}-"))
        .tempdir_in(scratch_root)?;
    let context = CaseContext {
        scale,
        wheat,
        scratch: scratch.path().to_owned(),
    };
    match name {
        deep_stack::NAME => deep_stack::run(&context),
        add_scale::NAME => add_scale::run(&context),
        transition_validate::NAME => transition_validate::run(&context),
        packed_append::NAME => packed_append::run(&context),
        content_repack_plan::NAME => content_repack_plan::run(&context),
        sharded_manifest::NAME => sharded_manifest::run(&context),
        git_export_notes::NAME => git_export_notes::run(&context),
        checkpoint_stack::NAME => checkpoint_stack::run(&context),
        _ => bail!("unknown case {name:?}; cases are {CASES:?}"),
    }
}

/// Runs `cases` (every case when empty), each as a child of `bench` (this binary),
/// and builds the `wheat-quick` report.
pub fn run(
    bench: &Path,
    wheat: &Path,
    scale: Scale,
    cases: &[String],
    scratch_root: &Path,
    checkout: &Checkout,
) -> Result<ProfileReportV1> {
    let selected: Vec<&str> = if cases.is_empty() {
        CASES.to_vec()
    } else {
        cases
            .iter()
            .map(|case| {
                CASES
                    .iter()
                    .copied()
                    .find(|known| known == case)
                    .with_context(|| format!("unknown case {case:?}; cases are {CASES:?}"))
            })
            .collect::<Result<_>>()?
    };

    let started = Instant::now();
    let mut measurements = Vec::new();
    let mut rules = Vec::new();
    let mut parameters = host::parameters(checkout, scratch_root)?;
    parameters.insert("scale".into(), json!(scale.name()));
    let mut failures = 0_u32;
    let mut peak = 0.0_f64;
    for case in &selected {
        eprintln!("case {case}: starting");
        let mut command = Command::new(bench);
        crate::isolate(&mut command)
            .args(["case", case, "--scale", scale.name()])
            .arg("--wheat")
            .arg(wheat)
            .arg("--scratch")
            .arg(scratch_root);
        let captured = measure::run_showing_stderr(&mut command, scratch_root)?;
        let mut timing = captured.timing;
        let output = if captured.status.success() {
            serde_json::from_slice::<CaseOutput>(&captured.stdout)
                .with_context(|| format!("parse case {case} output"))
        } else {
            Err(anyhow::anyhow!(
                "case {case} exited with {}",
                captured.status
            ))
        };
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                eprintln!(
                    "case {case}: FAILED after {:.1} s: {error:#}",
                    timing.wall_seconds
                );
                failures += 1;
                peak = peak.max(timing.peak_rss_bytes);
                continue;
            }
        };
        timing.peak_rss_bytes = case_peak_rss(timing.peak_rss_bytes, &output.measurements);
        peak = peak.max(timing.peak_rss_bytes);
        eprintln!(
            "case {case}: done in {:.1} s, peak RSS {:.1} MiB",
            timing.wall_seconds,
            timing.peak_rss_bytes / (1024.0 * 1024.0)
        );
        measurements.push(Measurement::new(
            format!("{case}.total.wall"),
            timing.wall_seconds,
            "s",
        ));
        measurements.push(Measurement::new(
            format!("{case}.total.peak_rss"),
            timing.peak_rss_bytes,
            "bytes",
        ));
        measurements.extend(output.measurements);
        if scale == Scale::Full {
            rules.extend(output.rules);
        }
        parameters.insert((*case).to_owned(), json!(output.parameters));
    }

    measurements.extend([
        Measurement::new("run.total_time", started.elapsed().as_secs_f64(), "s"),
        Measurement::new("process.peak_rss", peak, "bytes"),
        Measurement::new("suite.cases", selected.len() as f64, "count"),
        Measurement::new("suite.case_failures", f64::from(failures), "count"),
    ]);
    rules.push(ContractRule::at_most("suite.case_failures", 0.0, "count"));
    let report = ProfileReportV1 {
        schema_version: 1,
        profile: PROFILE.into(),
        workload: Workload {
            name: PROFILE.into(),
            parameters,
        },
        contract: Contract { rules },
        measurements,
    };
    report.check()?;
    Ok(report)
}

/// A case's whole-process peak: what `wait4` reported, or the largest phase peak the
/// case measured itself if that is higher. [`measure::PhasePeak`] resets the kernel
/// mark that `wait4` reads, so after it `wait4` only sees the peak since the last
/// reset.
fn case_peak_rss(reaped: f64, measurements: &[Measurement]) -> f64 {
    measurements
        .iter()
        .filter(|measurement| measurement.name.ends_with(".peak_rss"))
        .map(|measurement| measurement.value)
        .fold(reaped, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_peak_covers_the_phase_peaks_it_reported() {
        let measurements = [
            Measurement::new("c.plan.peak_rss", 300.0, "bytes"),
            Measurement::new("c.plan.wall", 900.0, "s"),
            Measurement::new("c.prepare.peak_rss", 200.0, "bytes"),
        ];
        assert_eq!(case_peak_rss(250.0, &measurements), 300.0);
        assert_eq!(case_peak_rss(400.0, &measurements), 400.0);
        assert_eq!(case_peak_rss(100.0, &[]), 100.0);
    }
}
