//! Each integration-test binary links most of Wheat, and one per file once filled the
//! disk, so every crate builds its integration tests as one binary (`CLAUDE.md`):
//! `autotests = false` and a single `[[test]]` named `integration`, whose harness
//! `tests/harnesses/integration.rs` names each file under `tests/` as a module. This
//! test fails on a crate that builds any other test binary, and on a file under
//! `tests/` the harness leaves out, which with `autotests = false` would never be
//! compiled. Binaries that genuinely need their own process image are listed in
//! [`OWN_BINARIES`] with the reason.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

const HARNESS: &str = "tests/harnesses/integration.rs";

/// `(crate directory relative to crates/, test name, why it is a binary of its own)`.
const OWN_BINARIES: &[(&str, &str, &str)] = &[];

#[test]
fn each_crate_builds_its_integration_tests_as_one_binary() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut manifests = Vec::new();
    collect_manifests(&crates, &mut manifests);
    assert!(
        manifests.len() > 10,
        "found only {} manifests",
        manifests.len()
    );

    let mut report = String::new();
    let mut own_binaries_seen = Vec::new();
    for manifest in &manifests {
        let directory = manifest.parent().unwrap();
        let relative = directory
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(manifest).unwrap();
        let tests = test_targets(&text);
        for (name, path) in &tests {
            if name == "integration" && path == HARNESS {
                continue;
            }
            if OWN_BINARIES
                .iter()
                .any(|(krate, own, _)| *krate == relative && own == name)
            {
                own_binaries_seen.push((relative.clone(), name.clone()));
                continue;
            }
            writeln!(
                report,
                "crates/{relative}/Cargo.toml: test binary `{name}` ({path}); make its file a \
                 module of crates/{relative}/{HARNESS} instead"
            )
            .unwrap();
        }

        let sources = test_sources(&directory.join("tests"));
        if sources.is_empty() {
            continue;
        }
        if !text.lines().any(|line| line.trim() == "autotests = false") {
            writeln!(
                report,
                "crates/{relative}/Cargo.toml: lacks `autotests = false`, so Cargo builds each \
                 file under tests/ as a binary of its own"
            )
            .unwrap();
        }
        let harness = fs::read_to_string(directory.join(HARNESS)).unwrap_or_default();
        if !tests
            .iter()
            .any(|(name, path)| name == "integration" && path == HARNESS)
        {
            writeln!(
                report,
                "crates/{relative}/Cargo.toml: lacks the `integration` [[test]] at {HARNESS}"
            )
            .unwrap();
        }
        for source in &sources {
            let module = format!("#[path = \"../{source}\"]");
            if !harness.lines().any(|line| line.trim() == module) {
                writeln!(
                    report,
                    "crates/{relative}/tests/{source} is not a module of crates/{relative}/\
                     {HARNESS}, so it is never compiled; add `{module}` and its `mod` line there"
                )
                .unwrap();
            }
        }
    }
    assert!(
        report.is_empty(),
        "integration tests outside their crate's one test binary:\n{report}"
    );
    // Every listed exception still matches a binary, so the list cannot rot.
    for (krate, name, _) in OWN_BINARIES {
        assert!(
            own_binaries_seen
                .iter()
                .any(|(seen, own)| seen == krate && own == name),
            "stale OWN_BINARIES entry {krate} `{name}`"
        );
    }
}

/// The manifests under `directory`, skipping build output.
fn collect_manifests(directory: &Path, manifests: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().unwrap() != "target" {
                collect_manifests(&path, manifests);
            }
        } else if path.file_name().unwrap() == "Cargo.toml" {
            manifests.push(path);
        }
    }
}

/// The `(name, path)` of each `[[test]]` in a manifest.
fn test_targets(manifest: &str) -> Vec<(String, String)> {
    let mut targets = Vec::new();
    let mut in_test = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_test = line == "[[test]]";
            if in_test {
                targets.push((String::new(), String::new()));
            }
        } else if in_test {
            let target = targets.last_mut().unwrap();
            if let Some(value) = string_value(line, "name") {
                target.0 = value;
            } else if let Some(value) = string_value(line, "path") {
                target.1 = value;
            }
        }
    }
    targets
}

fn string_value(line: &str, key: &str) -> Option<String> {
    let value = line
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    Some(value.trim_matches('"').to_owned())
}

/// What a harness names as a module: each `.rs` file directly under `tests/` and each
/// directory's `mod.rs`, other than the harnesses' own.
fn test_sources(tests: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(tests) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "harnesses" {
                sources.push(format!("{name}/mod.rs"));
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(name);
        }
    }
    sources.sort();
    sources
}
