//! Committed result history under `bench/results/<profile>/`, and comparing two runs.
//!
//! Gothab keeps the uploaded profiles but does not compare runs, so every local run
//! also writes one small tab-separated file (tabs shown as spaces here):
//!
//! ```text
//! # wheat-bench results v1
//! # profile  wheat-quick
//! # stamp    20260925-101500
//! # cpu      AMD Ryzen 9 7950X 16-Core Processor
//! # ...every workload parameter...
//! measurement              value  unit
//! deep_stack.reapply.wall  1.234  s
//! ```
//!
//! One row per measurement keeps diffs readable when cases are added or removed.
//! `latest.tsv` beside the dated files is a copy of the newest one and is not committed.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::report::{Measurement, ProfileReportV1};

const MAGIC: &str = "# wheat-bench results v1";
const COLUMNS: &str = "measurement\tvalue\tunit";
/// Header fields that are expected to differ between two runs.
const PER_RUN_FIELDS: &[&str] = &["stamp", "rev", "dirty"];

/// One saved run.
#[derive(Clone, Debug, PartialEq)]
pub struct Results {
    /// `profile`, `stamp`, then every workload parameter, in file order.
    pub header: Vec<(String, String)>,
    pub measurements: Vec<Measurement>,
}

impl Results {
    pub fn from_report(report: &ProfileReportV1, stamp: &str) -> Self {
        let mut header = vec![
            ("profile".to_owned(), report.profile.clone()),
            ("stamp".to_owned(), stamp.to_owned()),
        ];
        header.extend(
            report
                .workload
                .parameters
                .iter()
                .map(|(key, value)| (key.clone(), header_value(value))),
        );
        Self {
            header,
            measurements: report.measurements.clone(),
        }
    }

    pub fn field(&self, name: &str) -> Option<&str> {
        self.header
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_str()))
    }

    pub fn to_tsv(&self) -> Result<String> {
        let mut text = format!("{MAGIC}\n");
        for (key, value) in &self.header {
            ensure!(
                !key.contains(['\t', '\n']) && !value.contains(['\t', '\n']),
                "header field {key:?} contains a tab or newline"
            );
            writeln!(text, "# {key}\t{value}")?;
        }
        writeln!(text, "{COLUMNS}")?;
        for measurement in &self.measurements {
            writeln!(
                text,
                "{}\t{}\t{}",
                measurement.name, measurement.value, measurement.unit
            )?;
        }
        Ok(text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        ensure!(
            lines.next() == Some(MAGIC),
            "not a wheat-bench results file (first line must be `{MAGIC}`)"
        );
        let mut header = Vec::new();
        let mut measurements = Vec::new();
        let mut in_rows = false;
        for line in lines {
            if !in_rows {
                if line == COLUMNS {
                    in_rows = true;
                    continue;
                }
                let (key, value) = line
                    .strip_prefix("# ")
                    .and_then(|field| field.split_once('\t'))
                    .with_context(|| format!("malformed header line {line:?}"))?;
                header.push((key.to_owned(), value.to_owned()));
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(name), Some(value), Some(unit), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                bail!("malformed measurement line {line:?}");
            };
            measurements.push(Measurement::new(
                name,
                value
                    .parse()
                    .with_context(|| format!("value of {name} is not a number"))?,
                unit,
            ));
        }
        ensure!(in_rows, "results file has no `{COLUMNS}` line");
        Ok(Self {
            header,
            measurements,
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(&fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
            .with_context(|| format!("parse {}", path.display()))
    }

    /// Writes `<results>/<profile>/<stamp>-<rev>.tsv` and copies it to `latest.tsv`.
    /// Returns the dated file.
    pub fn save(&self, results: &Path) -> Result<PathBuf> {
        let profile = self.field("profile").context("results have no profile")?;
        let stamp = self.field("stamp").context("results have no stamp")?;
        let rev = self.field("rev").context("results have no rev")?;
        let directory = results.join(profile);
        fs::create_dir_all(&directory)?;
        let text = self.to_tsv()?;
        let dated = directory.join(format!("{stamp}-{rev}.tsv"));
        fs::write(&dated, &text)?;
        fs::write(directory.join("latest.tsv"), &text)?;
        Ok(dated)
    }
}

/// Strings as they are; anything else as compact JSON.
fn header_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A table of every measurement in either run: old value, new value, and the change.
/// Header fields that differ (other than stamp, rev and dirty) come first as warnings,
/// because timings from different machines or sizes don't compare.
pub fn compare(old: &Results, new: &Results) -> String {
    let mut text = String::new();
    let keys: BTreeSet<&str> = old
        .header
        .iter()
        .chain(&new.header)
        .map(|(key, _)| key.as_str())
        .collect();
    for key in keys {
        let (before, after) = (old.field(key), new.field(key));
        if PER_RUN_FIELDS.contains(&key) || before == after {
            continue;
        }
        let _ = writeln!(
            text,
            "warning: {key} differs: {} -> {}",
            before.unwrap_or("-"),
            after.unwrap_or("-")
        );
    }
    let _ = writeln!(
        text,
        "comparing {} ({}) -> {} ({})",
        old.field("rev").unwrap_or("?"),
        old.field("stamp").unwrap_or("?"),
        new.field("rev").unwrap_or("?"),
        new.field("stamp").unwrap_or("?"),
    );

    let by_name = |results: &Results| -> BTreeMap<String, Measurement> {
        results
            .measurements
            .iter()
            .map(|measurement| (measurement.name.clone(), measurement.clone()))
            .collect()
    };
    let (before, after) = (by_name(old), by_name(new));
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let width = names.iter().map(|name| name.len()).max().unwrap_or(0);
    for name in names {
        let (old_value, new_value) = (before.get(name), after.get(name));
        let change = match (old_value, new_value) {
            (Some(old), Some(new)) if old.unit != new.unit => "unit changed".to_owned(),
            (Some(old), Some(new)) if old.value != 0.0 => {
                format!("{:+.1}%", (new.value - old.value) / old.value * 100.0)
            }
            (Some(_), Some(_)) => "-".to_owned(),
            (None, Some(_)) => "new".to_owned(),
            (Some(_), None) => "gone".to_owned(),
            (None, None) => unreachable!("every name comes from one side"),
        };
        let _ = writeln!(
            text,
            "{name:<width$}  {:>14}  {:>14}  {change}",
            old_value.map_or_else(|| "-".to_owned(), display),
            new_value.map_or_else(|| "-".to_owned(), display),
        );
    }
    text
}

fn display(measurement: &Measurement) -> String {
    match measurement.unit.as_str() {
        "bytes" => format!("{:.1} MiB", measurement.value / (1024.0 * 1024.0)),
        "s" => format!("{:.3} s", measurement.value),
        unit => format!("{:.3} {unit}", measurement.value),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::report::{Contract, Workload};

    fn report(cpu: &str, wall: f64) -> ProfileReportV1 {
        ProfileReportV1 {
            schema_version: 1,
            profile: "wheat-quick".into(),
            workload: Workload {
                name: "wheat-quick".into(),
                parameters: BTreeMap::from([
                    ("rev".into(), json!("abc123")),
                    ("cpu".into(), json!(cpu)),
                    ("sizes".into(), json!({"files": 20_000})),
                ]),
            },
            contract: Contract { rules: vec![] },
            measurements: vec![
                Measurement::new("add.wall", wall, "s"),
                Measurement::new("add.peak_rss", 64.0 * 1024.0 * 1024.0, "bytes"),
                Measurement::new("odd.value", 0.1 + 0.2, "count"),
            ],
        }
    }

    #[test]
    fn results_round_trip_through_the_file_format() {
        let results = Results::from_report(&report("CPU A", 1.5), "20260925-101500");
        let text = results.to_tsv().unwrap();
        assert!(text.starts_with("# wheat-bench results v1\n# profile\twheat-quick\n"));
        assert!(text.contains("# sizes\t{\"files\":20000}\n"));
        assert_eq!(Results::parse(&text).unwrap(), results);
    }

    #[test]
    fn saving_writes_a_dated_file_and_latest() {
        let directory = tempfile::tempdir().unwrap();
        let results = Results::from_report(&report("CPU A", 1.5), "20260925-101500");
        let dated = results.save(directory.path()).unwrap();
        assert_eq!(
            dated,
            directory
                .path()
                .join("wheat-quick/20260925-101500-abc123.tsv")
        );
        assert_eq!(Results::read(&dated).unwrap(), results);
        assert_eq!(
            Results::read(&directory.path().join("wheat-quick/latest.tsv")).unwrap(),
            results
        );
    }

    #[test]
    fn malformed_files_are_refused() {
        for text in [
            "",
            "measurement\tvalue\tunit\n",
            "# wheat-bench results v1\n# profile\n",
            "# wheat-bench results v1\nmeasurement\tvalue\tunit\na\tnot-a-number\ts\n",
            "# wheat-bench results v1\nmeasurement\tvalue\tunit\na\t1\n",
            "# wheat-bench results v1\n# profile\tp\n",
        ] {
            assert!(Results::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn compare_shows_changes_and_warns_about_different_hosts() {
        let old = Results::from_report(&report("CPU A", 2.0), "20260925-101500");
        let mut new_report = report("CPU B", 1.5);
        new_report.measurements.pop();
        new_report
            .measurements
            .push(Measurement::new("new.value", 3.0, "count"));
        let new = Results::from_report(&new_report, "20260926-101500");
        let text = compare(&old, &new);
        assert!(
            text.contains("warning: cpu differs: CPU A -> CPU B"),
            "{text}"
        );
        assert!(!text.contains("stamp differs"), "{text}");
        let line = |name: &str| {
            text.lines()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} in {text}"))
                .to_owned()
        };
        assert!(line("add.wall").ends_with("-25.0%"), "{text}");
        assert!(line("add.wall").contains("2.000 s"), "{text}");
        assert!(line("add.peak_rss").contains("64.0 MiB"), "{text}");
        assert!(line("add.peak_rss").ends_with("+0.0%"), "{text}");
        assert!(line("new.value").ends_with("new"), "{text}");
        assert!(line("odd.value").ends_with("gone"), "{text}");
    }
}
