//! The version 1 performance-profile report that a Gothab workflow's `actions/profile`
//! step ingests.
//!
//! Wheat must not depend on gothab crates, so this is its own copy of the JSON shape
//! (only the parts this profile writes). Gothab validates every uploaded report again
//! on ingestion; the checks here catch mistakes before a long run is wasted.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Units the ingestion side accepts.
const UNITS: &[&str] = &["ms", "s", "bytes", "1/s", "percent", "count", "cores"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileReportV1 {
    pub(crate) schema_version: u16,
    pub(crate) profile: String,
    pub(crate) workload: Workload,
    pub(crate) contract: Contract,
    pub(crate) measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workload {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) parameters: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Contract {
    #[serde(default)]
    pub(crate) rules: Vec<ContractRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContractRule {
    pub(crate) measurement: String,
    pub(crate) comparison: Comparison,
    pub(crate) value: f64,
    pub(crate) unit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Comparison {
    Lte,
    Gte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Measurement {
    pub(crate) name: String,
    pub(crate) value: f64,
    pub(crate) unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ContractOutcome {
    Passed,
    Failed { violations: Vec<Violation> },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Violation {
    pub(crate) measurement: String,
    pub(crate) comparison: Comparison,
    pub(crate) actual: f64,
    pub(crate) threshold: f64,
    pub(crate) unit: String,
}

impl ProfileReportV1 {
    /// The compact JSON to upload, after checking the report is well formed.
    pub(crate) fn canonical_json(&self) -> Result<Vec<u8>> {
        self.check()?;
        Ok(serde_json::to_vec(self)?)
    }

    /// Checks every contract rule against its measurement.
    pub(crate) fn evaluate(&self) -> Result<ContractOutcome> {
        let measurements = self.check()?;
        let violations = self
            .contract
            .rules
            .iter()
            .filter_map(|rule| {
                let actual = measurements[rule.measurement.as_str()].value;
                let satisfied = match rule.comparison {
                    Comparison::Lte => actual <= rule.value,
                    Comparison::Gte => actual >= rule.value,
                };
                (!satisfied).then(|| Violation {
                    measurement: rule.measurement.clone(),
                    comparison: rule.comparison,
                    actual,
                    threshold: rule.value,
                    unit: rule.unit.clone(),
                })
            })
            .collect::<Vec<_>>();
        Ok(if violations.is_empty() {
            ContractOutcome::Passed
        } else {
            ContractOutcome::Failed { violations }
        })
    }

    /// Well-formedness: version 1, known units, finite values, unique measurement names,
    /// and every rule naming a measurement with the same unit. Returns the measurements
    /// by name.
    fn check(&self) -> Result<HashMap<&str, &Measurement>> {
        ensure!(
            self.schema_version == 1,
            "profile report schema version must be 1, not {}",
            self.schema_version
        );
        let mut measurements = HashMap::with_capacity(self.measurements.len());
        for measurement in &self.measurements {
            check_value(&measurement.name, measurement.value, &measurement.unit)?;
            if measurements
                .insert(measurement.name.as_str(), measurement)
                .is_some()
            {
                bail!("duplicate measurement `{}`", measurement.name);
            }
        }
        for rule in &self.contract.rules {
            check_value(&rule.measurement, rule.value, &rule.unit)?;
            let Some(measurement) = measurements.get(rule.measurement.as_str()) else {
                bail!(
                    "contract rule names unknown measurement `{}`",
                    rule.measurement
                );
            };
            ensure!(
                measurement.unit == rule.unit,
                "contract rule for `{}` is in {} but the measurement is in {}",
                rule.measurement,
                rule.unit,
                measurement.unit
            );
        }
        Ok(measurements)
    }
}

fn check_value(name: &str, value: f64, unit: &str) -> Result<()> {
    ensure!(
        value.is_finite(),
        "`{name}` is not a finite number: {value}"
    );
    ensure!(UNITS.contains(&unit), "`{name}` has unknown unit `{unit}`");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(measurements: Vec<Measurement>, rules: Vec<ContractRule>) -> ProfileReportV1 {
        ProfileReportV1 {
            schema_version: 1,
            profile: "p".into(),
            workload: Workload {
                name: "p".into(),
                parameters: BTreeMap::new(),
            },
            contract: Contract { rules },
            measurements,
        }
    }

    fn seconds(name: &str, value: f64) -> Measurement {
        Measurement {
            name: name.into(),
            value,
            unit: "s".into(),
        }
    }

    fn at_most(name: &str, value: f64, unit: &str) -> ContractRule {
        ContractRule {
            measurement: name.into(),
            comparison: Comparison::Lte,
            value,
            unit: unit.into(),
        }
    }

    #[test]
    fn the_json_matches_the_ingested_shape() {
        let report = report(vec![seconds("run", 2.0)], vec![at_most("run", 3.0, "s")]);
        let json: Value = serde_json::from_slice(&report.canonical_json().unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema_version": 1,
                "profile": "p",
                "workload": {"name": "p", "parameters": {}},
                "contract": {"rules": [
                    {"measurement": "run", "comparison": "lte", "value": 3.0, "unit": "s"}
                ]},
                "measurements": [{"name": "run", "value": 2.0, "unit": "s"}],
            })
        );
    }

    #[test]
    fn rules_pass_and_fail_on_their_threshold() {
        let rules = vec![at_most("run", 3.0, "s")];
        assert_eq!(
            report(vec![seconds("run", 3.0)], rules.clone())
                .evaluate()
                .unwrap(),
            ContractOutcome::Passed
        );
        let ContractOutcome::Failed { violations } =
            report(vec![seconds("run", 4.0)], rules).evaluate().unwrap()
        else {
            panic!("a value over the threshold must fail");
        };
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].actual, 4.0);
    }

    #[test]
    fn malformed_reports_are_refused() {
        let cases = [
            report(vec![seconds("run", f64::NAN)], vec![]),
            report(vec![seconds("run", 1.0), seconds("run", 2.0)], vec![]),
            report(vec![seconds("run", 1.0)], vec![at_most("other", 1.0, "s")]),
            report(
                vec![seconds("run", 1.0)],
                vec![at_most("run", 1.0, "bytes")],
            ),
            report(
                vec![Measurement {
                    name: "run".into(),
                    value: 1.0,
                    unit: "minutes".into(),
                }],
                vec![],
            ),
        ];
        for case in cases {
            assert!(case.canonical_json().is_err(), "{case:?}");
        }
    }
}
