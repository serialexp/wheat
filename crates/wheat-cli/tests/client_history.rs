#![cfg(target_os = "linux")]

//! The desktop client's reads, against a repository the `wheat` command built.

use std::{fs, path::Path, process::Command};

use wheat_client::{history, workspace_lifecycle};

fn ok(root: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn field(output: &std::process::Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    ok(root.path(), config.path(), &["init"]);
    ok(
        root.path(),
        config.path(),
        &["config", "set", "actor.name", "Test"],
    );
    ok(
        root.path(),
        config.path(),
        &["config", "set", "actor.email", "test@example.invalid"],
    );
    (root, config)
}

#[test]
fn history_lists_the_working_views_changes_with_their_revision_counts() {
    let (root, config) = setup();
    let first = field(
        &ok(
            root.path(),
            config.path(),
            &["change", "new", "-m", "first change"],
        ),
        "change",
    );
    fs::write(root.path().join("a"), "one\n").unwrap();
    ok(root.path(), config.path(), &["add", "a", "-m", "add a"]);
    fs::write(root.path().join("a"), "two\n").unwrap();
    let revised = ok(root.path(), config.path(), &["add", "a", "-m", "revise a"]);
    let current = field(&revised, "revision");

    let second = field(
        &ok(
            root.path(),
            config.path(),
            &["change", "new", "-m", "second change"],
        ),
        "change",
    );
    fs::write(root.path().join("b"), "b\n").unwrap();
    ok(root.path(), config.path(), &["add", "b", "-m", "add b"]);

    let entries = history::read(root.path(), None).unwrap();
    let first_entry = entries
        .iter()
        .find(|entry| entry.change.to_string() == first)
        .unwrap();
    let second_entry = entries
        .iter()
        .find(|entry| entry.change.to_string() == second)
        .unwrap();
    assert_eq!(first_entry.title, "first change");
    assert_eq!(first_entry.message, "revise a");
    assert_eq!(first_entry.revision.to_string(), current);
    assert_eq!(second_entry.title, "second change");
    assert_eq!(second_entry.message, "add b");
    assert!(
        first_entry.generation > second_entry.generation,
        "the twice-revised change has more revisions"
    );
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].committed_at.seconds >= pair[1].committed_at.seconds),
        "newest commit first"
    );

    let chain = history::revisions(root.path(), None, first_entry.revision, usize::MAX).unwrap();
    assert_eq!(chain.len() as u64, first_entry.generation + 1);
    assert_eq!(chain[0].revision, first_entry.revision);
    assert_eq!(chain[0].message, "revise a");
    assert_eq!(chain[0].author.name, "Test");
    let limited = history::revisions(root.path(), None, first_entry.revision, 1).unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn workspace_entries_mark_the_current_workspace() {
    let (root, config) = setup();
    let entries = workspace_lifecycle::entries(root.path(), None).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].current);
    assert_eq!(
        entries[0].path.canonicalize().unwrap(),
        root.path().canonicalize().unwrap()
    );
    drop(config);
}
