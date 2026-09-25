//! The `wheat-gothab-workflow` report: one wall time and peak RSS per phase, each
//! phase with its own wall-time ceiling.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{
    measure::Timing,
    report::{Comparison, Contract, ContractRule, Measurement, ProfileReportV1, Workload},
};

pub const PROFILE: &str = "wheat-gothab-workflow";
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

pub(crate) fn build_report(
    timings: &[(String, Timing)],
    parameters: BTreeMap<String, Value>,
    storage_bytes: u64,
    reachable_commits: u64,
    total_seconds: f64,
    peak_rss_bytes: f64,
) -> Result<ProfileReportV1> {
    let import_wall = timings
        .iter()
        .find(|(name, _)| name == "import.initial_materialize")
        .context("import timing is missing")?
        .1
        .wall_seconds;
    let mut measurements = Vec::with_capacity(timings.len() * 2 + 4);
    let mut rules = Vec::with_capacity(timings.len() + 2);
    for (name, timing) in timings {
        measurements.push(Measurement {
            name: format!("phase.{name}.wall"),
            value: timing.wall_seconds,
            unit: "s".into(),
        });
        measurements.push(Measurement {
            name: format!("phase.{name}.peak_rss"),
            value: timing.peak_rss_bytes,
            unit: "bytes".into(),
        });
        rules.push(ContractRule {
            measurement: format!("phase.{name}.wall"),
            comparison: Comparison::Lte,
            value: phase_wall_limit(name)?,
            unit: "s".into(),
        });
    }
    measurements.extend([
        Measurement {
            name: "import.commits_per_second".into(),
            value: reachable_commits as f64 / import_wall.max(f64::MIN_POSITIVE),
            unit: "1/s".into(),
        },
        Measurement {
            name: "run.total_time".into(),
            value: total_seconds,
            unit: "s".into(),
        },
        Measurement {
            name: "process.peak_rss".into(),
            value: peak_rss_bytes,
            unit: "bytes".into(),
        },
        Measurement {
            name: "storage.bytes".into(),
            value: storage_bytes as f64,
            unit: "bytes".into(),
        },
    ]);
    rules.extend([
        ContractRule {
            // Baseline: 3,246.56s locally; the pinned runner needs about 25%
            // longer. Keep bounded headroom while preserving a regression gate.
            measurement: "run.total_time".into(),
            comparison: Comparison::Lte,
            value: 5_400.0,
            unit: "s".into(),
        },
        ContractRule {
            measurement: "process.peak_rss".into(),
            comparison: Comparison::Lte,
            value: GIB,
            unit: "bytes".into(),
        },
    ]);
    // Per-phase RSS measurements intentionally have no individual rules: the
    // aggregate process.peak_rss rule bounds every one. Throughput and storage
    // are diagnostic measurements until representative baselines exist.
    Ok(ProfileReportV1 {
        schema_version: 1,
        profile: PROFILE.into(),
        workload: Workload {
            name: PROFILE.into(),
            parameters,
        },
        contract: Contract { rules },
        measurements,
    })
}

fn phase_wall_limit(name: &str) -> Result<f64> {
    // These are ratcheted from the 2026-09-14 local and pinned-runner Gothab
    // baselines rather than aspirational targets. CI must enforce a truthful
    // passing contract; later performance work can lower ceilings with evidence.
    let limit = match name {
        "import.initial_materialize" => 3_300.0,
        "push.working" | "validate.complete" => 600.0,
        "materialize.conflicted_working" | "materialize.resolved_working" => 420.0,
        "status.initial_clean"
        | "log.initial"
        | "diff.readme_edit"
        | "config.actor_name"
        | "config.actor_email"
        | "change.ancestor_create"
        | "capture.ancestor_paths"
        | "change.descendant_create"
        | "capture.descendant_paths"
        | "revise.ancestor_conflict"
        | "conflicts.inspect"
        | "resolve.id_free"
        | "status.resolved_clean"
        | "reopen.nested_status" => 30.0,
        _ => anyhow::bail!("measured phase {name} has no explicit wall-time rule"),
    };
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::ContractOutcome;
    use serde_json::json;

    fn all_timings(seconds: f64) -> Vec<(String, Timing)> {
        [
            "import.initial_materialize",
            "status.initial_clean",
            "log.initial",
            "diff.readme_edit",
            "config.actor_name",
            "config.actor_email",
            "change.ancestor_create",
            "capture.ancestor_paths",
            "change.descendant_create",
            "capture.descendant_paths",
            "revise.ancestor_conflict",
            "conflicts.inspect",
            "materialize.conflicted_working",
            "resolve.id_free",
            "materialize.resolved_working",
            "status.resolved_clean",
            "push.working",
            "reopen.nested_status",
            "validate.complete",
        ]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                Timing {
                    wall_seconds: seconds,
                    peak_rss_bytes: 2048.0,
                },
            )
        })
        .collect()
    }

    #[test]
    fn report_is_valid_and_contract_evaluates() {
        let report = build_report(
            &all_timings(1.0),
            BTreeMap::from([("samples".into(), json!(1))]),
            42,
            10,
            123.0,
            456.0,
        )
        .unwrap();
        report.canonical_json().unwrap();
        assert_eq!(report.evaluate().unwrap(), ContractOutcome::Passed);
        assert_eq!(report.contract.rules.len(), all_timings(1.0).len() + 2);
        assert_eq!(
            report
                .measurements
                .iter()
                .find(|measurement| measurement.name == "run.total_time")
                .unwrap()
                .value,
            123.0
        );
        assert_eq!(
            report
                .measurements
                .iter()
                .find(|measurement| measurement.name == "process.peak_rss")
                .unwrap()
                .value,
            456.0
        );
    }

    #[test]
    fn slow_phases_fail_the_contract() {
        let report =
            build_report(&all_timings(700.0), BTreeMap::new(), 42, 10, 700.0, 2048.0).unwrap();
        report.canonical_json().unwrap();
        assert!(matches!(
            report.evaluate().unwrap(),
            ContractOutcome::Failed { .. }
        ));
    }

    #[test]
    fn unknown_measured_phase_requires_an_explicit_rule() {
        let error = build_report(
            &[
                (
                    "import.initial_materialize".into(),
                    Timing {
                        wall_seconds: 1.0,
                        peak_rss_bytes: 1.0,
                    },
                ),
                (
                    "new.phase".into(),
                    Timing {
                        wall_seconds: 1.0,
                        peak_rss_bytes: 1.0,
                    },
                ),
            ],
            BTreeMap::new(),
            0,
            0,
            1.0,
            1.0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no explicit wall-time rule"));
    }
}
