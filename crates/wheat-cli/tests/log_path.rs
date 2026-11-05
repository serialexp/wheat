#![cfg(target_os = "linux")]

//! `wheat log PATH`: the changes that touch a file or a directory, a page at a time.

use std::{fs, path::Path, process::Command};

fn run(root: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .env("WHEAT_STORE_HOME", config.join("stores"))
        .args(args)
        .output()
        .unwrap()
}

fn ok(root: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let output = run(root, config, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A new change writing `path`; returns its ID.
fn change(root: &Path, config: &Path, path: &str, bytes: &str) -> String {
    let created = ok(root, config, &["change", "new", "-m", path]);
    let id = String::from_utf8_lossy(&created.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("change "))
        .unwrap()
        .to_owned();
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, bytes).unwrap();
    ok(root, config, &["add", path, "-m", path]);
    id
}

/// `wheat log` with `args` as JSON: the listed change IDs and the next page's
/// `--before`.
fn log(root: &Path, config: &Path, args: &[&str]) -> (Vec<String>, Option<u64>) {
    let mut arguments = vec!["log", "--format", "json"];
    arguments.extend_from_slice(args);
    let output = ok(root, config, &arguments);
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let changes = output["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["change_id"].as_str().unwrap().to_owned())
        .collect();
    (changes, output["result"]["next"].as_u64())
}

#[test]
fn log_with_a_path_lists_the_changes_that_touch_it_newest_first() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (root, config) = (root.path(), config.path());
    ok(root, config, &["init"]);
    ok(root, config, &["config", "set", "actor.name", "Test"]);
    ok(
        root,
        config,
        &["config", "set", "actor.email", "test@example.invalid"],
    );
    let first = change(root, config, "src/a", "one\n");
    let second = change(root, config, "src/b", "b\n");
    let third = change(root, config, "top", "top\n");
    let fourth = change(root, config, "src/a", "two\n");

    assert_eq!(
        log(root, config, &["src/a"]),
        (vec![fourth.clone(), first.clone()], None)
    );
    assert_eq!(log(root, config, &["top"]), (vec![third], None));
    assert_eq!(log(root, config, &["absent"]), (vec![], None));
    // A directory's changes, two at a time.
    assert_eq!(
        log(root, config, &["src/", "--limit", "2"]),
        (vec![fourth, second], Some(1))
    );
    assert_eq!(
        log(root, config, &["src", "--limit", "2", "--before", "1"]),
        (vec![first], None)
    );

    let human = ok(root, config, &["log", "src", "--limit", "1"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.ends_with("older changes: --before 3\n"), "{human}");

    for refused in [
        &["log", "../src"][..],
        &["log", "src//a"],
        &["log", "src", "--before", "5"],
        &["log", "--limit", "2"],
    ] {
        let output = run(root, config, refused);
        assert!(!output.status.success(), "{refused:?} was accepted");
    }
}
