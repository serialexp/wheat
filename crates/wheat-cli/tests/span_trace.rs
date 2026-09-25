#![cfg(target_os = "linux")]
//! `WHEAT_TRACE` and `WHEAT_TRACE_SUMMARY` report where a command spent its time.

use std::{fs, path::Path, process::Command};

fn wheat(home: &Path, arguments: &[&str], environment: &[(&str, &Path)]) -> std::process::Output {
    let config = home.join("config").join("wheat");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .args(arguments)
        .env("WHEAT_STORE_HOME", home.join("stores"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("WHEAT_TRACE")
        .env_remove("WHEAT_TRACE_SUMMARY")
        .envs(environment.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn a_traced_command_writes_a_timeline_and_a_summary() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let trace = home.path().join("trace.json");
    let output = wheat(
        home.path(),
        &["init", workspace.to_str().unwrap()],
        &[
            ("WHEAT_TRACE", &trace),
            ("WHEAT_TRACE_SUMMARY", Path::new("1")),
        ],
    );

    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.starts_with("wheat span summary\n"), "{summary}");
    assert!(
        summary
            .lines()
            .any(|line| line.trim_start().starts_with("command ")),
        "{summary}"
    );

    let document: serde_json::Value = serde_json::from_slice(&fs::read(&trace).unwrap()).unwrap();
    let command = document["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["name"] == "command")
        .expect("the command span");
    assert_eq!(command["ph"], "X");
    assert_eq!(command["args"]["command"], "init");
}

#[test]
fn an_untraced_command_reports_nothing() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let output = wheat(home.path(), &["init", workspace.to_str().unwrap()], &[]);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
