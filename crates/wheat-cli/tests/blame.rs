#![cfg(target_os = "linux")]

//! `wheat blame PATH`: which change last wrote each line of a file.

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

/// A new change writing `bytes` to `path`; returns its ID.
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

/// `wheat blame` with `args` as JSON: each line's change ID, none when unresolved.
fn blame(root: &Path, config: &Path, args: &[&str]) -> Vec<Option<String>> {
    let mut arguments = vec!["blame", "--format", "json"];
    arguments.extend_from_slice(args);
    let output = ok(root, config, &arguments);
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = &output["result"];
    let mut lines = Vec::new();
    for range in result["ranges"].as_array().unwrap() {
        let origin = range["origin"].as_u64().map(|index| {
            result["origins"][index as usize]["change_id"]
                .as_str()
                .unwrap()
                .to_owned()
        });
        for _ in 0..range["len"].as_u64().unwrap() {
            lines.push(origin.clone());
        }
    }
    assert_eq!(lines.len() as u64, result["lines"].as_u64().unwrap());
    assert_eq!(
        result["complete"].as_bool().unwrap(),
        lines.iter().all(Option::is_some)
    );
    lines
}

#[test]
fn blame_names_the_change_that_wrote_each_line() {
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
    let first = change(root, config, "src/a", "one\ntwo\nthree\n");
    change(root, config, "src/b", "b\n");
    let third = change(root, config, "src/a", "one\nTWO\nthree\nfour\n");
    let fourth = change(root, config, "src/a", "zero\none\nTWO\nthree\nfour\n");

    let some = |id: &String| Some(id.clone());
    assert_eq!(
        blame(root, config, &["src/a"]),
        [
            some(&fourth),
            some(&first),
            some(&third),
            some(&first),
            some(&third)
        ]
    );
    // Walking only the newest change leaves the older lines unresolved.
    assert_eq!(
        blame(root, config, &["src/a", "--limit", "1"]),
        [some(&fourth), None, None, None, None]
    );

    let human = ok(root, config, &["blame", "src/a"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.starts_with(&format!("{fourth} Test ")), "{human}");
    assert!(human.contains("  1  zero\n"), "{human}");
    assert!(human.contains("  5  four\n"), "{human}");

    change(root, config, "bin", "\0\u{1}");
    for refused in [
        &["blame", "../src/a"][..],
        &["blame", "src//a"],
        &["blame", "src"],
        &["blame", "absent"],
        &["blame", "bin"],
    ] {
        let output = run(root, config, refused);
        assert!(!output.status.success(), "{refused:?} was accepted");
    }
}
