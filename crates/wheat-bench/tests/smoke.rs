//! Runs the whole `wheat-quick` suite at smoke size through the real binary, so every
//! case keeps working between real benchmark runs.
//!
//! Needs the `wheat` binary next to `wheat-bench` in the target directory. `just test`
//! builds every workspace binary; on its own, run
//! `cargo build -p wheat-cli --bin wheat` first.
#![cfg(target_os = "linux")]

use std::{path::Path, process::Command};

use wheat_bench::{
    quick::{CASES, PROFILE},
    report::ProfileReportV1,
};

#[test]
fn every_quick_case_runs_at_smoke_size() {
    let bench = Path::new(env!("CARGO_BIN_EXE_wheat-bench"));
    let wheat = bench.with_file_name("wheat");
    assert!(
        wheat.is_file(),
        "no wheat binary at {}; build it with `cargo build -p wheat-cli --bin wheat`",
        wheat.display()
    );
    let directory = tempfile::tempdir().unwrap();
    let report_path = directory.path().join("profile.json");

    let output = Command::new(bench)
        .args(["quick", "--scale", "smoke", "--no-save", "--output"])
        .arg(&report_path)
        .env("WHEAT_BENCH_SCRATCH_ROOT", directory.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wheat-bench failed with {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let report: ProfileReportV1 =
        serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    let measurements = report.check().unwrap();
    assert_eq!(report.profile, PROFILE);
    assert_eq!(report.workload.parameters["scale"], "smoke");
    assert_eq!(measurements["suite.case_failures"].value, 0.0);
    assert_eq!(measurements["suite.cases"].value, CASES.len() as f64);
    // At smoke size only the suite rule is checked.
    assert_eq!(report.contract.rules.len(), 1);

    let mut expected = vec![
        "run.total_time",
        "process.peak_rss",
        "deep_stack.fitting_overhead",
        "deep_stack.moving_overhead",
        "add_scale.validate.wall",
        "transition_validate.transition_share",
        "packed_append.n32.objects_per_second",
        "content_repack_plan.prepare.peak_rss",
        "sharded_manifest.index_and_one_shard.wall",
        "publish_content.publish_server.peak_rss",
        "publish_content.clone.peak_rss",
        "import_resume_wide.resume.wall",
    ];
    let totals = CASES
        .iter()
        .flat_map(|case| {
            [
                format!("{case}.total.wall"),
                format!("{case}.total.peak_rss"),
            ]
        })
        .collect::<Vec<_>>();
    expected.extend(totals.iter().map(String::as_str));
    for name in expected {
        assert!(measurements.contains_key(name), "missing {name}");
    }
    for case in CASES {
        assert!(
            report.workload.parameters.contains_key(*case),
            "missing parameters for {case}"
        );
        // The case's whole-process peak covers the phase peaks inside it.
        let total = measurements[format!("{case}.total.peak_rss").as_str()].value;
        for measurement in &report.measurements {
            if measurement.name.starts_with(&format!("{case}."))
                && measurement.name.ends_with(".peak_rss")
            {
                assert!(
                    measurement.value <= total,
                    "{} exceeds the case peak",
                    measurement.name
                );
            }
        }
    }
}
