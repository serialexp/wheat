#![cfg(target_os = "linux")]

use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn run(workspace: &Path, stores: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .env("WHEAT_STORE_HOME", stores)
        .env("XDG_CONFIG_HOME", stores.join("config"))
        .output()
        .unwrap()
}

fn ok(workspace: &Path, stores: &Path, arguments: &[&str]) -> std::process::Output {
    let output = run(workspace, stores, arguments);
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn store_path(workspace: &Path) -> std::path::PathBuf {
    let metadata: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    metadata["store"]["path"].as_str().unwrap().into()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("git");
    let workspace = root.path().join("workspace");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    fs::write(source.join("file.txt"), b"one\ntwo\nthree\n").unwrap();
    git(&source, &["add", "file.txt"]);
    git(&source, &["commit", "-m", "root"]);
    ok(
        root.path(),
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    (root, stores, workspace)
}

#[test]
fn analyze_is_repeatable_read_only_and_census_is_exhaustive() {
    let (root, stores, workspace) = fixture();
    let store = store_path(&workspace);
    let before = fs::read(store.join("repository.head")).unwrap();
    let args = [
        "--store",
        store.to_str().unwrap(),
        "--format",
        "json",
        "store",
        "analyze",
    ];
    let first = ok(root.path(), &stores, &args);
    let second = ok(root.path(), &stores, &args);
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(fs::read(store.join("repository.head")).unwrap(), before);
    let value: Value = serde_json::from_slice(&first.stdout).unwrap();
    let result = &value["result"];
    assert_eq!(
        result["total_disk_bytes"].as_u64().unwrap(),
        result["git_equivalent_bytes"].as_u64().unwrap()
            + result["wheat_specific_bytes"].as_u64().unwrap()
    );
    assert!(
        result["content_roles"]["whole_file_only_ids"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn analyze_rejects_malformed_authenticated_segment() {
    let (root, stores, workspace) = fixture();
    let store = store_path(&workspace);
    let segment = fs::read_dir(store.join("storage/packed/segments"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "segment"));
    if let Some(segment) = segment {
        let mut bytes = fs::read(segment.path()).unwrap();
        bytes[0] ^= 0xff;
        fs::write(segment.path(), bytes).unwrap();
        let output = run(
            root.path(),
            &stores,
            &["--store", store.to_str().unwrap(), "store", "analyze"],
        );
        assert!(
            !output.status.success(),
            "corruption must not produce an exact report"
        );
    }
}

fn content_role_physical_total(result: &Value) -> u64 {
    [
        "whole_file_only_physical_bytes",
        "guarded_fragment_only_physical_bytes",
        "whole_file_and_guarded_fragment_physical_bytes",
        "required_other_physical_bytes",
        "unreferenced_physical_bytes",
    ]
    .iter()
    .map(|key| result["content_roles"][key].as_u64().unwrap())
    .sum()
}

/// Total bytes of the selected content generation's shard packs.
fn selected_content_pack_bytes(store: &Path) -> u64 {
    let generations = store.join("storage/content-generations");
    let directory = fs::read_dir(&generations)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_type().unwrap().is_dir())
        .expect("repack must select a content generation");
    fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .map(|path| fs::metadata(path).unwrap().len())
        .sum()
}

/// Repack moves every file content out of segments/loose storage into the
/// selected content generation's pack. The role split must follow it there
/// instead of silently reporting zero physical bytes.
#[test]
fn analyze_attributes_repacked_content_generation_to_roles() {
    let (root, stores, workspace) = fixture();
    let store = store_path(&workspace);
    ok(
        root.path(),
        &stores,
        &["--store", store.to_str().unwrap(), "repack"],
    );
    let output = ok(
        root.path(),
        &stores,
        &[
            "--store",
            store.to_str().unwrap(),
            "--format",
            "json",
            "store",
            "analyze",
        ],
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = &value["result"];
    let pack_bytes = selected_content_pack_bytes(&store);
    assert!(pack_bytes > 0, "fixture content must be packed");
    assert!(
        result["content_roles"]["whole_file_only_physical_bytes"]
            .as_u64()
            .unwrap()
            > 0,
        "packed whole-file content must be attributed: {result}"
    );
    assert_eq!(
        result["content_roles"]["unreferenced_physical_bytes"]
            .as_u64()
            .unwrap(),
        0,
        "every packed content ID of a fresh import is retained: {result}"
    );
    assert!(
        content_role_physical_total(result) >= pack_bytes,
        "role split must cover the whole selected pack: {result}"
    );
}

#[test]
fn analyze_rejects_tampered_selected_content_manifest() {
    let (root, stores, workspace) = fixture();
    let store = store_path(&workspace);
    ok(
        root.path(),
        &stores,
        &["--store", store.to_str().unwrap(), "repack"],
    );
    let manifest = fs::read_dir(store.join("storage/content-generations"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "manifest")
        })
        .expect("repack must write a selected manifest");
    let mut bytes = fs::read(manifest.path()).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(manifest.path(), bytes).unwrap();
    let output = run(
        root.path(),
        &stores,
        &[
            "--store",
            store.to_str().unwrap(),
            "--format",
            "json",
            "store",
            "analyze",
        ],
    );
    assert!(
        !output.status.success(),
        "a tampered selected manifest must not produce an exact report\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Repacking an imported history stores older metadata as in-segment deltas;
/// the analyzer reports them and every metadata entry lands in one depth.
#[test]
fn analyze_reports_metadata_delta_chains_after_repack() {
    let root = tempfile::tempdir().unwrap();
    let stores = root.path().join("stores");
    let source = root.path().join("git");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(source.join("wide")).unwrap();
    git(&source, &["init", "-b", "main"]);
    for index in 0..32 {
        fs::write(
            source.join(format!("wide/file{index:02}.txt")),
            b"initial\n",
        )
        .unwrap();
    }
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "root"]);
    for step in 0..6 {
        fs::write(
            source.join(format!("wide/file{step:02}.txt")),
            format!("step {step}\n"),
        )
        .unwrap();
        git(&source, &["commit", "-am", &format!("step {step}")]);
    }
    ok(
        root.path(),
        &stores,
        &[
            "import",
            source.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    let store = store_path(&workspace);
    ok(
        root.path(),
        &stores,
        &["--store", store.to_str().unwrap(), "repack"],
    );
    let output = ok(
        root.path(),
        &stores,
        &[
            "--store",
            store.to_str().unwrap(),
            "--format",
            "json",
            "store",
            "analyze",
        ],
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = &value["result"];
    let deltas = result["metadata_delta_records"].as_u64().unwrap();
    assert!(
        deltas > 0,
        "older history must be stored as deltas: {result}"
    );
    let depths = result["metadata_entries_by_delta_depth"]
        .as_array()
        .unwrap()
        .iter()
        .map(|count| count.as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(depths.len(), 17);
    assert_eq!(depths[1..].iter().sum::<u64>(), deltas);
    let deepest = depths.iter().rposition(|count| *count > 0).unwrap();
    assert_eq!(result["metadata_max_delta_depth"], deepest as u64);
    assert!(deepest >= 1);
}
