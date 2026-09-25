#![cfg(target_os = "linux")]

#[path = "wheat_gothab_profile/mod.rs"]
mod wheat_gothab_profile;

use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use wheat_gothab_profile::{
    command::{BoundedOutput, ProfileRun, peak_process_rss_bytes},
    fixture::{
        create_exact_fixture, directory_size, git, git_clone, git_init_bare, select_paths,
        source_inventory,
    },
    report::write_report,
};

fn main() -> Result<()> {
    let full_run_started = Instant::now();
    const USAGE: &str = "usage: wheat_gothab_profile CORPUS [OUTPUT]\n\
        CORPUS is a full (not shallow) Git checkout to profile against, e.g. gothab.\n\
        The wheat binary is the one built next to this example \
        (target/release/wheat); set WHEAT_PROFILE_BINARY to use another.";
    let mut arguments = env::args_os().skip(1);
    let source = fs::canonicalize(arguments.next().context(USAGE)?)?;
    let output = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("profile.json"));
    ensure!(arguments.next().is_none(), USAGE);
    let wheat = wheat_binary()?;
    ensure!(
        wheat.is_file(),
        "release Wheat binary missing at {}; build it with `cargo build --release --bin wheat`",
        wheat.display()
    );
    ensure!(
        Path::new("/usr/bin/time").is_file(),
        "GNU /usr/bin/time is required"
    );

    let (
        corpus_sha,
        corpus_worktree_dirty,
        reachable_commits,
        tracked_files,
        git_objects,
        git_bytes,
    ) = source_inventory(&source)?;
    let selected = select_paths(&source)?;

    let scratch_parent = match env::var_os("WHEAT_PROFILE_SCRATCH_ROOT") {
        Some(path) => {
            let path = PathBuf::from(path);
            fs::create_dir_all(&path)?;
            path
        }
        None => source
            .parent()
            .context("source checkout has no parent")?
            .to_owned(),
    };
    let scratch = tempfile::Builder::new()
        .prefix("wheat-gothab-profile-")
        .tempdir_in(scratch_parent)?;
    let fixture = scratch.path().join("source.git");
    let workspace = scratch.path().join("workspace");
    let remote = scratch.path().join("remote.git");
    let clone = scratch.path().join("clone");
    // The fixture is deliberately the clean, reproducible Git corpus at HEAD, even
    // when the corpus checkout has uncommitted changes (`corpus_worktree_dirty`).
    create_exact_fixture(&source, &fixture, &corpus_sha)?;

    let mut run = ProfileRun::new(
        wheat,
        workspace.clone(),
        scratch.path().join("stores"),
        scratch.path().join("config"),
    );
    let fixture_s = path_text(&fixture)?;
    let workspace_s = path_text(&workspace)?;
    run.wheat_ok(
        "import.initial_materialize",
        scratch.path(),
        &[
            "import",
            fixture_s,
            workspace_s,
            "--branch",
            "refs/heads/main",
        ],
    )?;
    assert_clean_status(&run.json("status.initial_clean", &workspace, &["status"])?)?;
    let log = run.json("log.initial", &workspace, &["log"])?;
    ensure!(
        log["result"]["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()),
        "imported log is empty"
    );

    let readme = workspace.join(&selected.readme);
    let regular = workspace.join(&selected.regular);
    let executable = workspace.join(&selected.executable);
    let readme_preimage = fs::read(&readme)?;
    let regular_preimage = fs::read(&regular)?;
    let executable_preimage = fs::read(&executable)?;
    append(&readme, b"\nWheat Gothab profile diff edit.\n")?;
    let diff = run.json("diff.readme_edit", &workspace, &["diff"])?;
    ensure!(
        diff["result"]["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "README edit was absent from diff"
    );
    run.wheat_ok(
        "config.actor_name",
        &workspace,
        &["config", "set", "actor.name", "Wheat Profile"],
    )?;
    run.wheat_ok(
        "config.actor_email",
        &workspace,
        &[
            "config",
            "set",
            "actor.email",
            "wheat-profile@example.invalid",
        ],
    )?;
    fs::write(&readme, &readme_preimage)?;
    let original_regular = String::from_utf8(regular_preimage.clone())?;
    let ancestor_value = "Wheat profile ancestor value\n";
    let ancestor = run.wheat(
        "change.ancestor_create",
        &workspace,
        &[
            "change",
            "new",
            "--message",
            "Profile ancestor",
            "--path",
            &selected.regular,
            "--expected",
            &original_regular,
            "--value",
            ancestor_value,
        ],
    )?;
    let ancestor_change = output_field(&ancestor, "change")?;
    append(&executable, b"\n# Wheat profile executable edit\n")?;
    let symlink_path = workspace.join("wheat-profile-link");
    symlink(&selected.regular, &symlink_path)?;
    run.wheat_ok(
        "capture.ancestor_paths",
        &workspace,
        &[
            "add",
            "--message",
            "Capture regular executable and symlink",
            &selected.regular,
            &selected.executable,
            "wheat-profile-link",
        ],
    )?;

    let descendant = run.wheat(
        "change.descendant_create",
        &workspace,
        &[
            "change",
            "new",
            "--message",
            "Profile descendant",
            "--depends-on",
            &ancestor_change,
        ],
    )?;
    ensure!(
        output_field(&descendant, "change")? != ancestor_change,
        "descendant reused ancestor change identity"
    );
    fs::write(&regular, b"Wheat profile descendant value\n")?;
    fs::write(
        workspace.join("wheat-profile-added"),
        b"descendant addition\n",
    )?;
    run.wheat_ok(
        "capture.descendant_paths",
        &workspace,
        &[
            "add",
            "--message",
            "Capture descendant paths",
            &selected.regular,
            "wheat-profile-added",
        ],
    )?;

    fs::write(&regular, b"Wheat profile revised ancestor value\n")?;
    run.wheat_ok(
        "revise.ancestor_conflict",
        &workspace,
        &[
            "add",
            "--change",
            &ancestor_change,
            "--message",
            "Revise ancestor",
            &selected.regular,
        ],
    )?;
    let conflicts = run.json("conflicts.inspect", &workspace, &["conflicts"])?;
    let conflict_items = conflicts["result"]["conflicts"]
        .as_array()
        .context("conflicts result is not an array")?;
    ensure!(
        !conflict_items.is_empty(),
        "ancestor revision did not produce a descendant conflict"
    );
    let conflict_path = conflict_items
        .iter()
        .flat_map(|item| item["edits"].as_array().into_iter().flatten())
        .find_map(|edit| edit["path"].as_str())
        .context("conflict contained no path")?
        .to_owned();

    fs::write(&readme, &readme_preimage)?;
    fs::write(&regular, &regular_preimage)?;
    fs::write(&executable, &executable_preimage)?;
    let mut permissions = fs::metadata(&executable)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions)?;
    fs::remove_file(&symlink_path)?;
    fs::remove_file(workspace.join("wheat-profile-added"))?;
    run.wheat_ok(
        "materialize.conflicted_working",
        &workspace,
        &["materialize"],
    )?;
    ensure!(
        workspace.join("wheat-profile-added").is_file(),
        "successful descendant edit missing"
    );
    run.wheat_ok(
        "resolve.id_free",
        &workspace,
        &[
            "resolve",
            "--path",
            &conflict_path,
            "--value",
            "Wheat profile resolved value\n",
        ],
    )?;
    run.wheat_ok("materialize.resolved_working", &workspace, &["materialize"])?;
    assert_clean_status(&run.json("status.resolved_clean", &workspace, &["status"])?)?;

    git_init_bare(&remote)?;
    let remote_s = path_text(&remote)?;
    run.wheat_ok(
        "push.working",
        &workspace,
        &["push", remote_s, "--to", "refs/heads/main"],
    )?;
    git(&remote, &["fsck", "--strict"])?;
    let remote_tip = git(&remote, &["rev-parse", "refs/heads/main"])?;
    // Wheat re-encodes imported Git metadata, so projected commit IDs are not the
    // source IDs. The exact pinned source tree remains the compatibility anchor.
    let corpus_tree = git(&source, &["rev-parse", &format!("{corpus_sha}^{{tree}}")])?;
    let projected_commits = git(
        &remote,
        &["log", "--reverse", "--format=%H %T", &remote_tip],
    )?;
    let projected_base = projected_commits
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find_map(|(commit, tree)| (tree == corpus_tree).then_some(commit))
        .context("projected Git history does not contain the pinned corpus tree")?;
    let changed_paths = git(
        &remote,
        &["diff", "--name-only", projected_base, &remote_tip],
    )?;
    let mut actual_changed = changed_paths.lines().collect::<Vec<_>>();
    actual_changed.sort_unstable();
    let mut expected_changed = vec![
        selected.regular.as_str(),
        selected.executable.as_str(),
        "wheat-profile-added",
        "wheat-profile-link",
    ];
    expected_changed.sort_unstable();
    ensure!(
        actual_changed == expected_changed,
        "projected Git tree changed unexpected paths: {actual_changed:?}"
    );
    git_clone(&remote, &clone)?;
    git(&clone, &["diff", "--exit-code"])?;
    git(&clone, &["diff", "--cached", "--exit-code"])?;
    ensure!(
        fs::read(clone.join(&selected.regular))? == b"Wheat profile resolved value\n",
        "resolved content mismatch"
    );
    ensure!(
        fs::metadata(clone.join(&selected.executable))?.mode() & 0o111 != 0,
        "executable mode lost"
    );
    ensure!(
        fs::read_link(clone.join("wheat-profile-link"))? == Path::new(&selected.regular),
        "symlink target mismatch"
    );
    ensure!(
        fs::read(clone.join("wheat-profile-added"))? == b"descendant addition\n",
        "added content mismatch"
    );

    let nested = workspace.join(
        Path::new(&selected.regular)
            .parent()
            .unwrap_or(Path::new(".")),
    );
    assert_clean_status(&run.json("reopen.nested_status", &nested, &["status"])?)?;
    run.wheat_ok("validate.complete", &nested, &["validate"])?;

    let storage_bytes = directory_size(run.store_home())?;
    let total_seconds = full_run_started.elapsed().as_secs_f64();
    let peak_rss_bytes = peak_process_rss_bytes(&run.timings);
    write_report(
        &output,
        &run.timings,
        BTreeMap::from([
            ("corpus_sha".into(), json!(corpus_sha)),
            ("corpus_worktree_dirty".into(), json!(corpus_worktree_dirty)),
            ("reachable_commits".into(), json!(reachable_commits)),
            ("tracked_files".into(), json!(tracked_files)),
            ("git_objects".into(), json!(git_objects)),
            ("git_bytes".into(), json!(git_bytes)),
            ("release".into(), json!(cfg!(not(debug_assertions)))),
            ("samples".into(), json!(1)),
        ]),
        storage_bytes,
        reachable_commits,
        total_seconds,
        peak_rss_bytes,
    )?;
    Ok(())
}

/// The wheat binary to profile: `WHEAT_PROFILE_BINARY`, or the `wheat` that cargo built
/// into the same profile directory as this example (`target/release/examples/..` →
/// `target/release/wheat`).
fn wheat_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("WHEAT_PROFILE_BINARY") {
        return Ok(PathBuf::from(path));
    }
    let example = env::current_exe()?;
    let profile_directory = example
        .parent()
        .and_then(Path::parent)
        .context("the example binary is not inside a cargo target directory")?;
    Ok(profile_directory.join("wheat"))
}

fn assert_clean_status(value: &Value) -> Result<()> {
    ensure!(
        value["result"]["authority_advanced"] == false,
        "authority unexpectedly advanced"
    );
    ensure!(
        value["result"]["changes"] == json!([]),
        "workspace is not clean"
    );
    Ok(())
}

fn append(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

fn output_field(output: &BoundedOutput, name: &str) -> Result<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")).map(str::to_owned))
        .with_context(|| format!("missing {name} in Wheat output"))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("profile scratch path is not UTF-8")
}
