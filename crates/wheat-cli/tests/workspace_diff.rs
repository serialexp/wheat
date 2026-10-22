#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    process::{Command, Output},
};

use sandbox::{Sandbox, sandbox};

mod sandbox;

/// Diffs here must come out whole however busy the machine is.
const DIFF_TIME_LIMIT: &str = "--time-limit-ms=600000";

fn wheat(root: &std::path::Path, arguments: &[&str]) -> Output {
    let mut arguments = arguments.to_vec();
    if arguments.starts_with(&["change", "new"]) {
        arguments.extend(["--message", "test revision"]);
    }
    let config_home = root.parent().unwrap().join(format!(
        ".{}-test-config",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(config_home.join("wheat")).unwrap();
    fs::write(
        config_home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(root)
        .args(arguments)
        .env(
            "WHEAT_STORE_HOME",
            root.parent().unwrap().join(format!(
                ".{}-test-stores",
                root.file_name().unwrap().to_string_lossy()
            )),
        )
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .unwrap()
}

fn ok(root: &std::path::Path, arguments: &[&str]) -> Output {
    let output = wheat(root, arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup() -> Sandbox {
    let dir = sandbox();
    ok(dir.path(), &["init", dir.path().to_str().unwrap()]);
    ok(
        dir.path(),
        &["change", "new", "--path", "text", "--value", "one\ntwo"],
    );
    ok(
        dir.path(),
        &["change", "new", "--path", "binary", "--value", "before"],
    );
    ok(
        dir.path(),
        &[
            "change",
            "new",
            "--path",
            "link",
            "--value",
            "old-target",
            "--replacement-kind",
            "symlink",
        ],
    );
    ok(
        dir.path(),
        &["change", "new", "--path", "gone", "--value", "gone"],
    );
    ok(
        dir.path(),
        &["change", "new", "--path", "kind", "--value", "same"],
    );
    ok(dir.path(), &["materialize"]);
    dir
}

#[test]
fn workspace_diff_covers_text_binary_symlink_kind_delete_crlf_and_json_v1() {
    let dir = setup();
    fs::write(dir.path().join("text"), b"one\r\nchanged\r\n").unwrap();
    fs::write(dir.path().join("binary"), b"after\0bytes").unwrap();
    fs::remove_file(dir.path().join("link")).unwrap();
    symlink("new-target", dir.path().join("link")).unwrap();
    fs::remove_file(dir.path().join("gone")).unwrap();
    fs::write(dir.path().join("kind"), b"different\n").unwrap();
    let mut permissions = fs::metadata(dir.path().join("kind")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(dir.path().join("kind"), permissions).unwrap();

    let human = ok(dir.path(), &["diff", DIFF_TIME_LIMIT]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("-two\n\\ No newline at end of file"));
    assert!(human.contains("+changed\n"));
    assert!(human.contains("Binary files differ"));
    assert!(human.contains("Symlink target changed"));
    assert!(human.contains("File kind changed"));
    assert!(human.contains("-same"));
    assert!(human.contains("+different"));
    assert!(human.contains("File deleted"));
    assert!(human.contains("--- a/gone\n+++ /dev/null"));
    assert!(human.contains("@@ -1,1 +0,0 @@"));

    let json = ok(dir.path(), &["diff", DIFF_TIME_LIMIT, "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let document = &document["result"];
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["comparison"]["type"], "workspace_vs_pinned");
    assert_eq!(document["comparison"]["authority_advanced"], false);
    assert_eq!(document["files"].as_array().unwrap().len(), 5);
    let kind = document["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "kind")
        .unwrap();
    assert_eq!(kind["change"], "kind_changed");
    assert_eq!(kind["presentation"]["type"], "kind");
    assert!(
        kind["presentation"]["diff"]["hunks"]
            .as_array()
            .is_some_and(|hunks| !hunks.is_empty())
    );
}

#[test]
fn clean_workspace_diff_has_no_human_output() {
    let dir = setup();
    assert!(ok(dir.path(), &["diff", DIFF_TIME_LIMIT]).stdout.is_empty());
}

#[test]
fn workspace_diff_is_lexical_ignores_untracked_files_and_does_not_change_metadata() {
    let dir = setup();
    let metadata_before = fs::read(dir.path().join(".wheat/workspace.json")).unwrap();
    fs::write(dir.path().join("text"), b"changed\n").unwrap();
    fs::write(dir.path().join("binary"), b"changed\n").unwrap();
    fs::write(dir.path().join("untracked"), b"not part of phase one\n").unwrap();

    let output = ok(dir.path(), &["diff", DIFF_TIME_LIMIT, "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths = document["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["binary", "text"]);
    assert_eq!(
        fs::read(dir.path().join(".wheat/workspace.json")).unwrap(),
        metadata_before
    );
    assert_eq!(
        fs::read(dir.path().join("untracked")).unwrap(),
        b"not part of phase one\n"
    );
}

#[test]
fn oversized_workspace_text_is_identified_without_inline_capture() {
    let dir = setup();
    let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 7];
    bytes[64 * 1024 - 1] = b'\r';
    bytes[64 * 1024] = b'\n';
    fs::write(dir.path().join("text"), &bytes).unwrap();

    let output = ok(dir.path(), &["diff", DIFF_TIME_LIMIT, "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let file = document["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "text")
        .unwrap();
    assert_eq!(file["new"]["text"], true);
    assert_eq!(file["new"]["bytes"], bytes.len() - 1);
    assert_eq!(file["presentation"]["type"], "omitted");
    assert_eq!(file["presentation"]["reason"], "inline_bytes");

    let status = ok(dir.path(), &["status", "--format", "json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let tracked = status["result"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "text")
        .unwrap();
    assert_eq!(tracked["workspace"]["text"], true);
    assert_eq!(tracked["workspace"]["bytes"], bytes.len() - 1);
}

#[test]
fn oversized_workspace_binary_reports_raw_identity_and_length() {
    let dir = setup();
    let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
    bytes[10] = b'\r';
    let last = bytes.len() - 1;
    bytes[last] = 0xff;
    fs::write(dir.path().join("binary"), &bytes).unwrap();

    let output = ok(dir.path(), &["diff", DIFF_TIME_LIMIT, "--format", "json"]);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let file = document["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "binary")
        .unwrap();
    assert_eq!(file["new"]["text"], false);
    assert_eq!(file["new"]["bytes"], bytes.len());
    assert_eq!(file["presentation"]["type"], "binary");
}

#[test]
fn a_spent_time_limit_omits_line_diffs() {
    let dir = setup();
    fs::write(dir.path().join("text"), b"changed\n").unwrap();

    let output = ok(
        dir.path(),
        &["diff", "--time-limit-ms=0", "--format", "json"],
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let file = document["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "text")
        .unwrap();
    assert_eq!(file["presentation"]["type"], "omitted");
    assert_eq!(file["presentation"]["reason"], "deadline");
}

#[test]
fn diff_uses_the_pinned_tree_when_repository_authority_advances() {
    let dir = setup();
    ok(
        dir.path(),
        &[
            "change",
            "new",
            "--path",
            "text",
            "--expected",
            "one\ntwo",
            "--value",
            "authority value\n",
        ],
    );

    // The workspace still exactly matches the pinned materialization even though
    // the current working view now contains a different value.
    assert!(ok(dir.path(), &["diff", DIFF_TIME_LIMIT]).stdout.is_empty());
    let status = ok(dir.path(), &["status", "--format", "json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["result"]["authority_advanced"], true);
}

#[test]
fn diff_recovers_an_interrupted_workspace_operation_before_scanning() {
    let dir = setup();
    fs::write(dir.path().join("text"), b"discarded\n").unwrap();
    let interrupted = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(dir.path())
        .args(["restore", "text"])
        .env(
            "WHEAT_STORE_HOME",
            dir.path().parent().unwrap().join(format!(
                ".{}-test-stores",
                dir.path().file_name().unwrap().to_string_lossy()
            )),
        )
        .env(
            "WHEAT_TEST_MATERIALIZATION_FAIL_AFTER",
            "completion-persisted",
        )
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    assert!(dir.path().join(".wheat/materialization.json").exists());

    assert!(ok(dir.path(), &["diff", DIFF_TIME_LIMIT]).stdout.is_empty());
    assert!(!dir.path().join(".wheat/materialization.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn diff_obeys_the_workspace_operation_lock() {
    use std::os::fd::AsRawFd as _;

    let dir = setup();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join(".wheat/workspace.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let blocked = wheat(dir.path(), &["diff", DIFF_TIME_LIMIT]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("workspace is locked"));
}

#[test]
fn diff_validates_revision_and_view_forms() {
    let dir = setup();
    let invalid = wheat(
        dir.path(),
        &["diff", DIFF_TIME_LIMIT, "--revision", "anything"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid revision ID"));
    let incomplete = wheat(
        dir.path(),
        &["diff", DIFF_TIME_LIMIT, "--from", "label:main"],
    );
    assert!(!incomplete.status.success());
    assert!(incomplete.stdout.is_empty());
    let conflicting = wheat(
        dir.path(),
        &[
            "diff",
            DIFF_TIME_LIMIT,
            "--revision",
            "anything",
            "--from",
            "label:main",
            "--to",
            "label:main",
        ],
    );
    assert!(!conflicting.status.success());
    assert!(conflicting.stdout.is_empty());
    assert!(
        wheat(
            dir.path(),
            &[
                "diff",
                DIFF_TIME_LIMIT,
                "--from",
                "label:main",
                "--to",
                "label:main"
            ]
        )
        .status
        .success()
    );
}
