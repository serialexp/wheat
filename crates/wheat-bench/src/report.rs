//! The version 1 performance-profile report that a Gothab workflow's `actions/profile`
//! step ingests.
//!
//! Wheat must not depend on gothab crates, so this is its own copy of the JSON shape
//! (only the parts Wheat's profiles write). Gothab validates every uploaded report
//! again on ingestion (`gothab-profile/src/validate.rs`); [`ProfileReportV1::check`]
//! applies the same rules, so a bad name or an oversized report fails here, before a
//! long run is wasted.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Units the ingestion side accepts.
const UNITS: &[&str] = &["ms", "s", "bytes", "1/s", "percent", "count", "cores"];
/// Gothab's ingestion limits.
const MAX_REPORT_BYTES: usize = 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_MEASUREMENTS: usize = 2_000;
const MAX_CONTRACT_RULES: usize = 2_000;
const MAX_PARAMETERS: usize = 256;
const MAX_PARAMETER_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileReportV1 {
    pub schema_version: u16,
    pub profile: String,
    pub workload: Workload,
    pub contract: Contract,
    pub measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub name: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    #[serde(default)]
    pub rules: Vec<ContractRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRule {
    pub measurement: String,
    pub comparison: Comparison,
    pub value: f64,
    pub unit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Lte,
    Gte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub name: String,
    pub value: f64,
    pub unit: String,
}

impl Measurement {
    pub fn new(name: impl Into<String>, value: f64, unit: &str) -> Self {
        Self {
            name: name.into(),
            value,
            unit: unit.to_owned(),
        }
    }
}

impl ContractRule {
    pub fn at_most(measurement: impl Into<String>, value: f64, unit: &str) -> Self {
        Self {
            measurement: measurement.into(),
            comparison: Comparison::Lte,
            value,
            unit: unit.to_owned(),
        }
    }

    pub fn at_least(measurement: impl Into<String>, value: f64, unit: &str) -> Self {
        Self {
            measurement: measurement.into(),
            comparison: Comparison::Gte,
            value,
            unit: unit.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContractOutcome {
    Passed,
    Failed { violations: Vec<Violation> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub measurement: String,
    pub comparison: Comparison,
    pub actual: f64,
    pub threshold: f64,
    pub unit: String,
}

impl ProfileReportV1 {
    /// The compact JSON to upload, after checking the report is well formed.
    pub fn canonical_json(&self) -> Result<Vec<u8>> {
        self.check()?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "profile report is {} bytes, over Gothab's {MAX_REPORT_BYTES} byte limit",
            bytes.len()
        );
        Ok(bytes)
    }

    /// Checks every contract rule against its measurement.
    pub fn evaluate(&self) -> Result<ContractOutcome> {
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

    /// Gothab's well-formedness rules: version 1, identifier names within the length
    /// limit, bounded counts and parameter nesting, known units, finite values, unique
    /// measurements and rules, and every rule naming a measurement with the same unit.
    /// Returns the measurements by name.
    pub fn check(&self) -> Result<HashMap<&str, &Measurement>> {
        ensure!(
            self.schema_version == 1,
            "profile report schema version must be 1, not {}",
            self.schema_version
        );
        check_identifier("profile", &self.profile, false)?;
        check_identifier("workload.name", &self.workload.name, false)?;
        ensure!(
            self.workload.parameters.len() <= MAX_PARAMETERS,
            "more than {MAX_PARAMETERS} workload parameters"
        );
        for (key, value) in &self.workload.parameters {
            check_identifier("workload parameter", key, false)?;
            check_parameter(key, value, 0)?;
        }

        ensure!(
            self.measurements.len() <= MAX_MEASUREMENTS,
            "{} measurements, over Gothab's limit of {MAX_MEASUREMENTS}",
            self.measurements.len()
        );
        let mut measurements = HashMap::with_capacity(self.measurements.len());
        for measurement in &self.measurements {
            check_identifier("measurement", &measurement.name, true)?;
            check_value(&measurement.name, measurement.value, &measurement.unit)?;
            if measurements
                .insert(measurement.name.as_str(), measurement)
                .is_some()
            {
                bail!("duplicate measurement `{}`", measurement.name);
            }
        }

        ensure!(
            self.contract.rules.len() <= MAX_CONTRACT_RULES,
            "{} contract rules, over Gothab's limit of {MAX_CONTRACT_RULES}",
            self.contract.rules.len()
        );
        let mut ruled = HashSet::with_capacity(self.contract.rules.len());
        for rule in &self.contract.rules {
            check_identifier("contract rule", &rule.measurement, true)?;
            check_value(&rule.measurement, rule.value, &rule.unit)?;
            ensure!(
                ruled.insert(rule.measurement.as_str()),
                "duplicate contract rule for `{}`",
                rule.measurement
            );
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

/// Lowercase ASCII letters, digits, `_` and `-`; `dotted` names are `.`-separated
/// segments of those. At most [`MAX_NAME_BYTES`].
fn check_identifier(field: &str, value: &str, dotted: bool) -> Result<()> {
    ensure!(!value.is_empty(), "{field} name is empty");
    ensure!(
        value.len() <= MAX_NAME_BYTES,
        "{field} `{value}` is longer than {MAX_NAME_BYTES} bytes"
    );
    let segment_ok = |segment: &str| {
        !segment.is_empty()
            && segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
    };
    let ok = if dotted {
        value.split('.').all(segment_ok)
    } else {
        segment_ok(value)
    };
    ensure!(
        ok,
        "{field} `{value}` must be lowercase {}identifiers ([a-z0-9_-])",
        if dotted { "dot-separated " } else { "" }
    );
    Ok(())
}

fn check_parameter(field: &str, value: &Value, depth: usize) -> Result<()> {
    ensure!(
        depth <= MAX_PARAMETER_DEPTH,
        "workload parameter `{field}` nests deeper than {MAX_PARAMETER_DEPTH}"
    );
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
        Value::Number(number) => {
            ensure!(
                number.as_f64().is_some_and(f64::is_finite),
                "workload parameter `{field}` is not a finite number"
            );
            Ok(())
        }
        Value::Array(values) => {
            ensure!(
                values.len() <= MAX_PARAMETERS,
                "workload parameter `{field}` has more than {MAX_PARAMETERS} items"
            );
            values
                .iter()
                .try_for_each(|value| check_parameter(field, value, depth + 1))
        }
        Value::Object(values) => {
            ensure!(
                values.len() <= MAX_PARAMETERS,
                "workload parameter `{field}` has more than {MAX_PARAMETERS} keys"
            );
            values
                .values()
                .try_for_each(|value| check_parameter(field, value, depth + 1))
        }
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
        Measurement::new(name, value, "s")
    }

    #[test]
    fn the_json_matches_the_ingested_shape() {
        let report = report(
            vec![seconds("run", 2.0)],
            vec![ContractRule::at_most("run", 3.0, "s")],
        );
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
        let rules = vec![ContractRule::at_most("run", 3.0, "s")];
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

        let at_least = vec![ContractRule::at_least("run", 3.0, "s")];
        assert_eq!(
            report(vec![seconds("run", 3.0)], at_least.clone())
                .evaluate()
                .unwrap(),
            ContractOutcome::Passed
        );
        assert!(matches!(
            report(vec![seconds("run", 2.0)], at_least)
                .evaluate()
                .unwrap(),
            ContractOutcome::Failed { .. }
        ));
    }

    #[test]
    fn malformed_values_and_rules_are_refused() {
        let cases = [
            report(vec![seconds("run", f64::NAN)], vec![]),
            report(vec![seconds("run", 1.0), seconds("run", 2.0)], vec![]),
            report(
                vec![seconds("run", 1.0)],
                vec![ContractRule::at_most("other", 1.0, "s")],
            ),
            report(
                vec![seconds("run", 1.0)],
                vec![ContractRule::at_most("run", 1.0, "bytes")],
            ),
            report(vec![Measurement::new("run", 1.0, "minutes")], vec![]),
            report(
                vec![seconds("run", 1.0)],
                vec![
                    ContractRule::at_most("run", 1.0, "s"),
                    ContractRule::at_least("run", 0.5, "s"),
                ],
            ),
        ];
        for case in cases {
            assert!(case.canonical_json().is_err(), "{case:?}");
        }
    }

    #[test]
    fn names_follow_gothab_identifier_rules() {
        for good in ["run", "a.b_c.d-e", "phase.import.initial_materialize.wall"] {
            report(vec![seconds(good, 1.0)], vec![])
                .canonical_json()
                .unwrap();
        }
        let too_long = "a".repeat(MAX_NAME_BYTES + 1);
        for bad in [
            "Run",
            "a..b",
            ".a",
            "a.",
            "a b",
            "a/b",
            "",
            too_long.as_str(),
        ] {
            assert!(
                report(vec![seconds(bad, 1.0)], vec![])
                    .canonical_json()
                    .is_err(),
                "{bad:?}"
            );
        }

        let mut bad_profile = report(vec![], vec![]);
        bad_profile.profile = "wheat.quick".into();
        assert!(bad_profile.canonical_json().is_err());

        let mut bad_parameter = report(vec![], vec![]);
        bad_parameter
            .workload
            .parameters
            .insert("Sizes".into(), Value::from(1));
        assert!(bad_parameter.canonical_json().is_err());
    }

    #[test]
    fn counts_depth_and_size_are_bounded() {
        let many = (0..=MAX_MEASUREMENTS)
            .map(|index| seconds(&format!("m{index}"), 1.0))
            .collect();
        assert!(report(many, vec![]).canonical_json().is_err());

        let mut deep = Value::from(1);
        for _ in 0..=MAX_PARAMETER_DEPTH + 1 {
            deep = Value::Array(vec![deep]);
        }
        let mut nested = report(vec![], vec![]);
        nested.workload.parameters.insert("deep".into(), deep);
        assert!(nested.canonical_json().is_err());

        // Few enough measurements to pass the count limit, but over 1 MiB of JSON.
        let mut large = report(vec![], vec![]);
        large
            .workload
            .parameters
            .insert("blob".into(), Value::from("x".repeat(MAX_REPORT_BYTES)));
        assert!(large.canonical_json().is_err());
    }
}
