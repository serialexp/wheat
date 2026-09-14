#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

struct Harness {
    root: tempfile::TempDir,
    workspace: PathBuf,
    stores: PathBuf,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        Self {
            workspace: root.path().join("workspace"),
            stores: root.path().join("stores"),
            config: root.path().join("config"),
            root,
        }
    }

    fn wheat(&self, current_dir: &Path, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .current_dir(current_dir)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("wheat command runs in its own process")
    }

    fn ok(&self, current_dir: &Path, arguments: &[&str]) -> Output {
        let output = self.wheat(current_dir, arguments);
        assert!(
            output.status.success(),
            "wheat {arguments:?} failed (status {:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    fn json_ok(&self, current_dir: &Path, command_arguments: &[&str]) -> Value {
        let mut arguments = vec!["--format=json"];
        arguments.extend_from_slice(command_arguments);
        let output = self.ok(current_dir, &arguments);
        let document = json_document(&output);
        assert_eq!(document["ok"], true);
        document
    }

    fn store(&self) -> PathBuf {
        let workspace: Value = serde_json::from_slice(
            &fs::read(self.workspace.join(".wheat/workspace.json")).unwrap(),
        )
        .unwrap();
        PathBuf::from(
            workspace["store"]["path"]
                .as_str()
                .expect("workspace has a canonical store path"),
        )
    }
}

fn git(directory: Option<&Path>, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Phase One Importer")
        .env("GIT_AUTHOR_EMAIL", "importer@example.invalid")
        .env("GIT_COMMITTER_NAME", "Phase One Importer")
        .env("GIT_COMMITTER_EMAIL", "importer@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git command runs in its own process");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn json_document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "structured command wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "structured output must be exactly one line"
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["format"], "wheat-command-result");
    assert_eq!(document["version"], 1);
    document
}

fn output_field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| {
            panic!(
                "missing {name} field in {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
        .to_owned()
}

fn set_executable(path: &Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SnapshotEntry {
    Directory,
    Regular { bytes: Vec<u8>, executable: bool },
    Symlink { target: PathBuf },
}

fn filesystem_snapshot(
    root: &Path,
    omit_operational_store_artifacts: bool,
) -> Vec<(PathBuf, SnapshotEntry)> {
    fn visit(
        root: &Path,
        directory: &Path,
        omit_operational_store_artifacts: bool,
        snapshot: &mut Vec<(PathBuf, SnapshotEntry)>,
    ) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if omit_operational_store_artifacts
                && (relative == Path::new("storage/packed/catalog-locator.sqlite3")
                    || relative
                        .file_name()
                        .is_some_and(|name| name.as_encoded_bytes().ends_with(b".lock")))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).unwrap();
            let file_type = metadata.file_type();
            let value = if file_type.is_dir() {
                SnapshotEntry::Directory
            } else if file_type.is_file() {
                SnapshotEntry::Regular {
                    bytes: fs::read(&path).unwrap(),
                    executable: metadata.permissions().mode() & 0o111 != 0,
                }
            } else if file_type.is_symlink() {
                SnapshotEntry::Symlink {
                    target: fs::read_link(&path).unwrap(),
                }
            } else {
                panic!("unsupported filesystem entry at {}", path.display());
            };
            snapshot.push((relative, value));
            if file_type.is_dir() {
                visit(root, &path, omit_operational_store_artifacts, snapshot);
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(root, root, omit_operational_store_artifacts, &mut snapshot);
    snapshot
}

fn canonical_store_snapshot(root: &Path) -> Vec<(PathBuf, SnapshotEntry)> {
    filesystem_snapshot(root, true)
}

fn operational_store_artifacts(root: &Path) -> Vec<(PathBuf, SnapshotEntry)> {
    filesystem_snapshot(root, false)
        .into_iter()
        .filter(|(path, _)| {
            path == Path::new("storage/packed/catalog-locator.sqlite3")
                || path
                    .file_name()
                    .is_some_and(|name| name.as_encoded_bytes().ends_with(b".lock"))
        })
        .collect()
}

fn regular(bytes: &[u8], executable: bool) -> SnapshotEntry {
    SnapshotEntry::Regular {
        bytes: bytes.to_vec(),
        executable,
    }
}

fn content_id(bytes: &[u8]) -> Value {
    serde_json::to_value(wheat_core::ContentId::of(bytes)).unwrap()
}

fn assert_workspace_entries(workspace: &Path, expected: Vec<(&str, SnapshotEntry)>) {
    let actual = filesystem_snapshot(workspace, false)
        .into_iter()
        .filter(|(path, _)| path != Path::new(".wheat") && !path.starts_with(".wheat/"))
        .collect::<Vec<_>>();
    let expected = expected
        .into_iter()
        .map(|(path, entry)| (PathBuf::from(path), entry))
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "complete workspace entry snapshot differs"
    );
}

type RenderedDiffLine = (String, Option<u64>, Option<u64>, String);

fn reconstructed_diff_text(file: &Value) -> (String, Vec<RenderedDiffLine>) {
    let diff = &file["presentation"]["diff"];
    let text = diff["text"].as_str().unwrap();
    let mut old = String::new();
    let mut new = String::new();
    let mut lines = Vec::new();
    for hunk in diff["hunks"].as_array().unwrap() {
        for line in hunk["lines"].as_array().unwrap() {
            let start = line["text_start"].as_u64().unwrap() as usize;
            let end = line["text_end"].as_u64().unwrap() as usize;
            let slice = text.get(start..end).unwrap();
            let operation = line["operation"].as_str().unwrap();
            if operation != "insert" {
                old.push_str(slice);
            }
            if operation != "delete" {
                new.push_str(slice);
            }
            lines.push((
                operation.to_owned(),
                line["old_line"].as_u64(),
                line["new_line"].as_u64(),
                slice.to_owned(),
            ));
        }
    }
    (format!("{old}\0{new}"), lines)
}

#[test]
fn phase_one_routine_single_target_workflow_across_processes() {
    let harness = Harness::new();
    let source = harness.root.path().join("source");
    let import_remote = harness.root.path().join("import.git");
    let push_remote = harness.root.path().join("push.git");
    let clone = harness.root.path().join("clone");

    git(None, &["init", "-b", "main", source.to_str().unwrap()]);
    fs::write(source.join("shared.txt"), b"imported\n").unwrap();
    fs::write(source.join("tool.sh"), b"#!/bin/sh\necho imported\n").unwrap();
    set_executable(&source.join("tool.sh"));
    symlink("shared.txt", source.join("current")).unwrap();
    git(Some(&source), &["add", "shared.txt", "tool.sh", "current"]);
    git(Some(&source), &["commit", "-m", "import fixture"]);
    git(
        None,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            import_remote.to_str().unwrap(),
        ],
    );
    let remote_url = format!("file://{}", import_remote.display());

    let imported = harness.json_ok(
        harness.root.path(),
        &[
            "import",
            &remote_url,
            harness.workspace.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
    );
    assert_eq!(imported["command"], "import");
    assert!(!harness.workspace.join("repository.head").exists());
    assert!(harness.store().starts_with(&harness.stores));
    assert_workspace_entries(
        &harness.workspace,
        vec![
            (
                "current",
                SnapshotEntry::Symlink {
                    target: PathBuf::from("shared.txt"),
                },
            ),
            ("shared.txt", regular(b"imported\n", false)),
            ("tool.sh", regular(b"#!/bin/sh\necho imported\n", true)),
        ],
    );

    // Import currently materializes as a convenience; Phase 1 must also support an explicit switch.
    let materialized = harness.json_ok(&harness.workspace, &["switch", "main"]);
    assert_eq!(materialized["command"], "switch");
    assert_workspace_entries(
        &harness.workspace,
        vec![
            (
                "current",
                SnapshotEntry::Symlink {
                    target: PathBuf::from("shared.txt"),
                },
            ),
            ("shared.txt", regular(b"imported\n", false)),
            ("tool.sh", regular(b"#!/bin/sh\necho imported\n", true)),
        ],
    );
    let status_after_materialize = harness.json_ok(&harness.workspace, &["status"]);
    assert_eq!(status_after_materialize["result"]["schema_version"], 1);
    assert_eq!(
        status_after_materialize["result"]["authority_advanced"],
        false
    );
    assert_eq!(
        status_after_materialize["result"]["changes"],
        serde_json::json!([])
    );
    let pinned_view: wheat_core::ViewId =
        serde_json::from_value(status_after_materialize["result"]["materialized_view"].clone())
            .unwrap();
    assert_eq!(materialized["result"]["view"], pinned_view.to_string());
    for forbidden in [
        ".git",
        "repository.head",
        "store.json",
        "storage",
        "projection",
    ] {
        assert!(
            !harness.workspace.join(forbidden).exists(),
            "repository authority leaked into workspace at {forbidden}"
        );
    }

    for (key, value) in [
        ("actor.name", "Phase One Author"),
        ("actor.email", "phase-one@example.invalid"),
    ] {
        let configured = harness.json_ok(&harness.workspace, &["config", "set", key, value]);
        assert_eq!(configured["command"], "config.set");
    }

    let ancestor = harness.ok(
        &harness.workspace,
        &[
            "change",
            "new",
            "--message",
            "Ancestor title\n\nInitial revision body",
            "--description",
            "Acceptance change description",
            "--created-at",
            "2026-02-01T01:02:03+01:00",
            "--authored-at",
            "2026-02-02T02:03:04-02:00",
            "--committed-at",
            "2026-02-03T03:04:05Z",
        ],
    );
    let ancestor_change = output_field(&ancestor, "change");

    fs::write(harness.workspace.join("shared.txt"), b"ancestor one\n").unwrap();
    fs::write(
        harness.workspace.join("tool.sh"),
        b"#!/bin/sh\necho ancestor\n",
    )
    .unwrap();
    set_executable(&harness.workspace.join("tool.sh"));
    fs::remove_file(harness.workspace.join("current")).unwrap();
    symlink("tool.sh", harness.workspace.join("current")).unwrap();
    fs::write(
        harness.workspace.join("not-selected.txt"),
        b"never capture me\n",
    )
    .unwrap();

    let workspace_before_reads = filesystem_snapshot(&harness.workspace, false);
    let store_before_reads = canonical_store_snapshot(&harness.store());
    let operational_before_reads = operational_store_artifacts(&harness.store());
    assert!(
        operational_before_reads.iter().any(|(path, entry)| path
            == Path::new("storage/packed/catalog-locator.sqlite3")
            && matches!(entry, SnapshotEntry::Regular { .. })),
        "the rebuildable packed catalog locator must be observed rather than silently ignored"
    );
    assert!(
        operational_before_reads
            .iter()
            .all(|(_, entry)| matches!(entry, SnapshotEntry::Regular { .. }))
    );
    let metadata_before_reads = fs::read(harness.workspace.join(".wheat/workspace.json")).unwrap();
    let status = harness.json_ok(&harness.workspace, &["status"]);
    assert_eq!(status["command"], "status");
    assert_eq!(status["result"]["schema_version"], 1);
    assert_eq!(status["result"]["authority_advanced"], true);
    assert!(
        status["result"]["current_head_generation"]
            .as_u64()
            .unwrap()
            > status["result"]["materialized_head_generation"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        status["result"]["changes"],
        serde_json::json!([
            {
                "path": "current",
                "content_changed": true,
                "kind_changed": false,
                "deleted": false,
                "baseline": { "kind": "Symlink", "content": content_id(b"shared.txt"), "bytes": 10, "text": false },
                "workspace": { "kind": "Symlink", "content": content_id(b"tool.sh"), "bytes": 7, "text": false }
            },
            {
                "path": "shared.txt",
                "content_changed": true,
                "kind_changed": false,
                "deleted": false,
                "baseline": { "kind": "Regular", "content": content_id(b"imported\n"), "bytes": 9, "text": true },
                "workspace": { "kind": "Regular", "content": content_id(b"ancestor one\n"), "bytes": 13, "text": true }
            },
            {
                "path": "tool.sh",
                "content_changed": true,
                "kind_changed": false,
                "deleted": false,
                "baseline": { "kind": "Executable", "content": content_id(b"#!/bin/sh\necho imported\n"), "bytes": 24, "text": true },
                "workspace": { "kind": "Executable", "content": content_id(b"#!/bin/sh\necho ancestor\n"), "bytes": 24, "text": true }
            }
        ])
    );

    let diff = harness.json_ok(&harness.workspace, &["diff"]);
    assert_eq!(diff["command"], "diff");
    assert_eq!(diff["result"]["schema_version"], 1);
    assert_eq!(diff["result"]["comparison"]["type"], "workspace_vs_pinned");
    assert_eq!(
        diff["result"]["comparison"]["materialized_view"],
        status["result"]["materialized_view"]
    );
    assert_eq!(
        diff["result"]["comparison"]["materialized_head_generation"],
        status["result"]["materialized_head_generation"]
    );
    assert_eq!(
        diff["result"]["comparison"]["current_head_generation"],
        status["result"]["current_head_generation"]
    );
    assert_eq!(diff["result"]["comparison"]["authority_advanced"], true);
    let files = diff["result"]["files"].as_array().unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["current", "shared.txt", "tool.sh"]
    );
    for (file, status_change) in files
        .iter()
        .zip(status["result"]["changes"].as_array().unwrap())
    {
        assert_eq!(file["change"], "modified");
        assert_eq!(file["old"], status_change["baseline"]);
        assert_eq!(file["new"], status_change["workspace"]);
        assert_eq!(file["content_changed"], true);
        assert_eq!(file["kind_changed"], false);
        assert_eq!(file["truncated"], false);
    }
    assert_eq!(
        files[0]["presentation"],
        serde_json::json!({"type": "symlink"})
    );
    let (shared_text, shared_lines) = reconstructed_diff_text(&files[1]);
    assert_eq!(shared_text, "imported\n\0ancestor one\n");
    assert_eq!(
        shared_lines,
        [
            ("delete".into(), Some(1), None, "imported\n".into()),
            ("insert".into(), None, Some(1), "ancestor one\n".into()),
        ]
    );
    let (tool_text, tool_lines) = reconstructed_diff_text(&files[2]);
    assert_eq!(
        tool_text,
        "#!/bin/sh\necho imported\n\0#!/bin/sh\necho ancestor\n"
    );
    assert_eq!(
        tool_lines,
        [
            ("context".into(), Some(1), Some(1), "#!/bin/sh\n".into()),
            ("delete".into(), Some(2), None, "echo imported\n".into()),
            ("insert".into(), None, Some(2), "echo ancestor\n".into()),
        ]
    );
    assert_eq!(
        canonical_store_snapshot(&harness.store()),
        store_before_reads
    );
    let operational_after_reads = operational_store_artifacts(&harness.store());
    assert_eq!(
        operational_after_reads
            .iter()
            .map(|(path, entry)| (path, std::mem::discriminant(entry)))
            .collect::<Vec<_>>(),
        operational_before_reads
            .iter()
            .map(|(path, entry)| (path, std::mem::discriminant(entry)))
            .collect::<Vec<_>>(),
        "read commands may mutate locator/lock bytes, but must preserve their explicit artifact paths and types"
    );
    assert_eq!(
        filesystem_snapshot(&harness.workspace, false),
        workspace_before_reads
    );
    assert_eq!(
        fs::read(harness.workspace.join(".wheat/workspace.json")).unwrap(),
        metadata_before_reads
    );

    let ancestor_added = harness.json_ok(
        &harness.workspace,
        &[
            "add",
            "--message",
            "Capture regular executable and symlink",
            "--authored-at",
            "2026-02-04T04:05:06+03:00",
            "--committed-at",
            "2026-02-05T05:06:07Z",
            "shared.txt",
            "tool.sh",
            "current",
        ],
    );
    assert_eq!(ancestor_added["result"]["paths"], 3);
    let log = harness.json_ok(&harness.workspace, &["log"]);
    assert_eq!(log["command"], "log");
    let entries = log["result"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let entry = entries
        .iter()
        .find(|entry| entry["change_id"] == ancestor_change)
        .expect("log contains the selected ancestor change");
    assert_eq!(entry["change"]["title"], "Ancestor title");
    assert_eq!(
        entry["change"]["description"],
        "Acceptance change description"
    );
    assert_eq!(
        entry["change"]["creator"],
        serde_json::json!({"name": "Phase One Author", "email": "phase-one@example.invalid"})
    );
    assert_eq!(
        entry["change"]["created_at"],
        serde_json::json!({"seconds": 1_769_904_123, "offset_minutes": 60})
    );
    assert_eq!(entry["change"]["provenance"], Value::Null);
    assert!(entry["predecessor"].is_string());
    assert_eq!(
        entry["revision"]["message"],
        "Capture regular executable and symlink"
    );
    assert_eq!(
        entry["revision"]["author"],
        serde_json::json!({"name": "Phase One Author", "email": "phase-one@example.invalid"})
    );
    assert_eq!(entry["revision"]["committer"], entry["revision"]["author"]);
    assert_eq!(
        entry["revision"]["authored_at"],
        serde_json::json!({"seconds": 1_770_167_106, "offset_minutes": 180})
    );
    assert_eq!(
        entry["revision"]["committed_at"],
        serde_json::json!({"seconds": 1_770_267_967, "offset_minutes": 0})
    );
    assert_eq!(entry["revision"]["provenance"], Value::Null);
    assert_eq!(entry["dependencies"].as_array().unwrap().len(), 1);
    assert!(entry["dependencies"][0].as_str().is_some());
    assert!(entry["authored_against"].is_string());
    assert!(entry["resulting_tree"].is_string());
    assert_eq!(
        fs::read(harness.workspace.join("not-selected.txt")).unwrap(),
        b"never capture me\n"
    );
    let descendant = harness.ok(
        &harness.workspace,
        &["change", "new", "--message", "Descendant change"],
    );
    let descendant_change = output_field(&descendant, "change");
    fs::write(harness.workspace.join("shared.txt"), b"descendant value\n").unwrap();
    fs::create_dir(harness.workspace.join("nested")).unwrap();
    fs::write(
        harness.workspace.join("nested/created.txt"),
        b"created by descendant\n",
    )
    .unwrap();
    let descendant_added = harness.json_ok(
        &harness.workspace,
        &[
            "add",
            "--message",
            "Capture descendant and newly created path",
            "shared.txt",
            "nested/created.txt",
        ],
    );
    assert_eq!(descendant_added["result"]["paths"], 2);

    fs::write(harness.workspace.join("shared.txt"), b"ancestor revised\n").unwrap();
    let revised = harness.ok(
        &harness.workspace,
        &[
            "add",
            "--change",
            &ancestor_change,
            "--message",
            "Revise ancestor beneath descendant",
            "shared.txt",
        ],
    );
    let revised_text = String::from_utf8_lossy(&revised.stdout);
    assert!(revised_text.contains(&format!("reapplied {descendant_change}")));

    // JSON status intentionally has no conflict count; durable conflict detail is a separate contract.
    let conflicted_status = harness.json_ok(&harness.workspace, &["status"]);
    assert!(conflicted_status["result"]["effective_view"].is_array());
    let conflicts_first = harness.ok(&harness.workspace, &["conflicts", "--format=json"]);
    let conflicts_first_document = json_document(&conflicts_first);
    assert_eq!(conflicts_first_document["ok"], true);
    assert_eq!(conflicts_first_document["command"], "conflicts");
    assert_eq!(conflicts_first_document["result"]["schema_version"], 1);
    assert!(
        conflicts_first_document["result"]["head_generation"]
            .as_u64()
            .is_some()
    );
    let conflicts = conflicts_first_document["result"]["conflicts"]
        .as_array()
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["id"].as_array().unwrap().len(), 32);
    assert_eq!(conflicts[0]["base_view"].as_array().unwrap().len(), 32);
    assert_eq!(conflicts[0]["change_id"], descendant_change);
    let conflicted_revision: wheat_core::RevisionId = descendant_added["result"]["revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        conflicts[0]["revision_id"],
        serde_json::to_value(conflicted_revision).unwrap()
    );
    assert_eq!(conflicts[0]["application_id"].as_array().unwrap().len(), 32);
    let edits = conflicts[0]["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["id"].as_array().unwrap().len(), 32);
    assert_eq!(edits[0]["path"], "shared.txt");
    assert_eq!(edits[0]["target"]["kind"], "Regular");
    assert_eq!(
        edits[0]["target"]["content"],
        content_id(b"ancestor revised\n")
    );
    assert_ne!(edits[0]["reason"], Value::Null);
    let conflicts_reopened = harness.ok(&harness.workspace, &["conflicts", "--format=json"]);
    assert_eq!(conflicts_reopened.stdout, conflicts_first.stdout);

    // Restore the exact pinned imported view before moving the workspace pin. Materialization
    // refuses to discard local edits, including the ancestor bytes used for revision capture.
    fs::write(harness.workspace.join("shared.txt"), b"imported\n").unwrap();
    fs::write(
        harness.workspace.join("tool.sh"),
        b"#!/bin/sh\necho imported\n",
    )
    .unwrap();
    set_executable(&harness.workspace.join("tool.sh"));
    fs::remove_file(harness.workspace.join("current")).unwrap();
    symlink("shared.txt", harness.workspace.join("current")).unwrap();
    fs::remove_file(harness.workspace.join("nested/created.txt")).unwrap();

    // Conflicted working state remains materializable and includes successful descendant paths.
    harness.ok(&harness.workspace, &["materialize"]);
    assert_eq!(
        fs::read(harness.workspace.join("shared.txt")).unwrap(),
        b"ancestor revised\n"
    );
    assert_eq!(
        fs::read(harness.workspace.join("nested/created.txt")).unwrap(),
        b"created by descendant\n"
    );

    // The normal path infers the single conflicted immutable revision from the path.
    let resolved = harness.json_ok(
        &harness.workspace,
        &[
            "resolve",
            "--path",
            "shared.txt",
            "--value",
            "resolved value\n",
        ],
    );
    assert_eq!(resolved["command"], "resolve");
    assert_eq!(
        harness.json_ok(&harness.workspace, &["conflicts"])["result"]["conflicts"],
        serde_json::json!([])
    );
    harness.ok(&harness.workspace, &["materialize"]);
    let clean_before_push = harness.json_ok(&harness.workspace, &["status"]);
    assert_eq!(clean_before_push["result"]["authority_advanced"], false);
    assert_eq!(
        clean_before_push["result"]["changes"],
        serde_json::json!([])
    );
    let effective_view: wheat_core::ViewId =
        serde_json::from_value(clean_before_push["result"]["effective_view"].clone()).unwrap();
    let effective_view = effective_view.to_string();
    assert_eq!(
        harness.json_ok(&harness.workspace, &["conflicts"])["result"]["conflicts"],
        serde_json::json!([])
    );
    assert_workspace_entries(
        &harness.workspace,
        vec![
            (
                "current",
                SnapshotEntry::Symlink {
                    target: PathBuf::from("tool.sh"),
                },
            ),
            ("nested", SnapshotEntry::Directory),
            (
                "nested/created.txt",
                regular(b"created by descendant\n", false),
            ),
            ("not-selected.txt", regular(b"never capture me\n", false)),
            ("shared.txt", regular(b"resolved value\n", false)),
            ("tool.sh", regular(b"#!/bin/sh\necho ancestor\n", true)),
        ],
    );

    git(None, &["init", "--bare", push_remote.to_str().unwrap()]);
    let pushed = harness.json_ok(
        &harness.workspace,
        &[
            "push",
            push_remote.to_str().unwrap(),
            "--to",
            "refs/heads/main",
            "--source",
            &effective_view,
        ],
    );
    assert_eq!(pushed["command"], "push");
    assert_eq!(pushed["result"]["up_to_date"], false);
    assert_eq!(git(Some(&push_remote), &["fsck", "--strict"]), "");
    git(
        None,
        &[
            "clone",
            "--branch",
            "main",
            push_remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(clone.join("shared.txt")).unwrap(),
        b"resolved value\n"
    );
    assert_eq!(
        fs::read(clone.join("tool.sh")).unwrap(),
        b"#!/bin/sh\necho ancestor\n"
    );
    assert_ne!(
        fs::metadata(clone.join("tool.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(clone.join("current")).unwrap(),
        Path::new("tool.sh")
    );
    assert_eq!(
        fs::read(clone.join("nested/created.txt")).unwrap(),
        b"created by descendant\n"
    );
    assert!(!clone.join("not-selected.txt").exists());
    assert_eq!(git(Some(&clone), &["fsck", "--strict"]), "");
    assert!(git(Some(&clone), &["ls-files", "-s", "tool.sh"]).starts_with("100755 "));
    assert!(git(Some(&clone), &["ls-files", "-s", "current"]).starts_with("120000 "));

    // A fresh process discovers the workspace from a descendant and preserves its exact pin,
    // authority head, selected change, metadata, and tracked filesystem state.
    let expected_reopened = harness.json_ok(&harness.workspace, &["status"]);
    assert_eq!(expected_reopened["result"]["authority_advanced"], false);
    assert_eq!(
        expected_reopened["result"]["changes"],
        serde_json::json!([])
    );
    let pinned_metadata = fs::read(harness.workspace.join(".wheat/workspace.json")).unwrap();
    let metadata_document: Value = serde_json::from_slice(&pinned_metadata).unwrap();
    assert_eq!(metadata_document["format"], "wheat-workspace");
    assert_eq!(metadata_document["version"], 3);
    assert_eq!(metadata_document["active_label"], "main");
    assert_eq!(
        metadata_document["basis"],
        expected_reopened["result"]["basis"]
    );
    assert_eq!(
        metadata_document["selected_frontier"],
        expected_reopened["result"]["selected_frontier"]
    );
    assert_eq!(metadata_document["selected_change"], descendant_change);
    assert_eq!(
        metadata_document["pending_repository_operation"],
        Value::Null
    );
    assert_eq!(
        metadata_document["materialized"]["repository_head"]["generation"],
        expected_reopened["result"]["materialized_head_generation"]
    );
    assert_eq!(
        metadata_document["materialized"]["view"],
        expected_reopened["result"]["materialized_view"]
    );
    let head_path = harness.store().join("repository.head");
    let expected_head = fs::read(&head_path).unwrap();
    let expected_workspace = filesystem_snapshot(&harness.workspace, false);
    let nested = harness.workspace.join("nested/reopen/here");
    fs::create_dir_all(&nested).unwrap();
    let reopened = harness.json_ok(&nested, &["status"]);
    assert_eq!(reopened, expected_reopened);
    assert_eq!(
        reopened["result"]["materialized_view"],
        reopened["result"]["effective_view"]
    );
    assert_eq!(
        reopened["result"]["materialized_head_generation"],
        reopened["result"]["current_head_generation"]
    );
    assert_eq!(fs::read(&head_path).unwrap(), expected_head);
    assert_eq!(
        fs::read(harness.workspace.join(".wheat/workspace.json")).unwrap(),
        pinned_metadata
    );
    let workspace_after_reopen = filesystem_snapshot(&harness.workspace, false);
    assert_eq!(
        workspace_after_reopen
            .into_iter()
            .filter(|(path, _)| !path.starts_with("nested/reopen"))
            .collect::<Vec<_>>(),
        expected_workspace
    );

    let head_before_stale = fs::read(&head_path).unwrap();
    fs::write(harness.workspace.join("shared.txt"), b"stale attempt\n").unwrap();
    let stale = harness.wheat(
        &harness.workspace,
        &[
            "--format=json",
            "add",
            "--change",
            &descendant_change,
            "--predecessor",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "shared.txt",
        ],
    );
    assert_eq!(stale.status.code(), Some(3));
    let stale = json_document(&stale);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["command"], "add");
    assert_eq!(
        stale["error"],
        serde_json::json!({
            "category": "state",
            "code": "stale_state",
            "retryable": true,
            "message": "predecessor is not the exact current revision of the change; refresh and retry"
        })
    );
    assert_eq!(fs::read(head_path).unwrap(), head_before_stale);
}
