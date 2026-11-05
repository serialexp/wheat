//! Children measured through the `wheat-bench` launcher report their own peak, not
//! the size of the process that started them.
#![cfg(target_os = "linux")]

use std::{path::PathBuf, process::Command};

use wheat_bench::measure;

const MIB: f64 = 1024.0 * 1024.0;

#[test]
fn a_child_of_a_large_process_reports_its_own_peak() {
    let scratch = tempfile::tempdir().unwrap();
    // Without the launcher, every child of this process would report at least this.
    let large = vec![1_u8; 384 * 1024 * 1024];
    std::hint::black_box(&large);
    measure::measure_through(PathBuf::from(env!("CARGO_BIN_EXE_wheat-bench")));

    let mut small = Command::new("sh");
    small
        .args(["-c", "echo small; exit 3"])
        .env("WHEAT_BENCH_PROBE", "set");
    let captured = measure::run(&mut small, scratch.path()).unwrap();
    assert_eq!(captured.status.code(), Some(3));
    assert_eq!(captured.stdout, b"small\n");
    assert!(
        captured.timing.peak_rss_bytes < 64.0 * MIB,
        "{:?}",
        captured.timing
    );

    // The child's environment and directory reach it through the launcher, and its
    // own peak still counts.
    let mut grown = Command::new("sh");
    grown
        .args([
            "-c",
            "test \"$WHEAT_BENCH_PROBE\" = set && pwd && dd if=/dev/zero of=/dev/null bs=64M count=1 2>/dev/null",
        ])
        .env("WHEAT_BENCH_PROBE", "set")
        .current_dir(scratch.path());
    let captured = measure::run(&mut grown, scratch.path()).unwrap();
    captured.ensure_success("sh").unwrap();
    assert_eq!(
        PathBuf::from(String::from_utf8(captured.stdout).unwrap().trim_end()),
        scratch.path().canonicalize().unwrap()
    );
    assert!(
        (64.0 * MIB..256.0 * MIB).contains(&captured.timing.peak_rss_bytes),
        "{:?}",
        captured.timing
    );
    drop(large);
}

#[test]
fn a_command_the_launcher_cannot_start_is_an_error() {
    let scratch = tempfile::tempdir().unwrap();
    measure::measure_through(PathBuf::from(env!("CARGO_BIN_EXE_wheat-bench")));
    let error = measure::run(&mut Command::new("/nonexistent/program"), scratch.path())
        .err()
        .expect("no such program");
    assert!(
        format!("{error:#}").contains("/nonexistent/program"),
        "{error:#}"
    );
}
