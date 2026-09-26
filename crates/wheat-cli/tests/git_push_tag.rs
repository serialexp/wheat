#![cfg(target_os = "linux")]
//! `wheat push --tag`: annotated tags whose object IDs never change.
//!
//! The point of these tests is that the same label gives the same tag object and the
//! same commit wherever and whenever it is pushed, and that the commit is the one the
//! branch push already put on the remote.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

const TAGGED_AT: &str = "2026-09-26T12:00:00+09:00";

fn git(directory: Option<&Path>, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Wheat Test")
        .env("GIT_AUTHOR_EMAIL", "wheat@example.invalid")
        .env("GIT_COMMITTER_NAME", "Wheat Test")
        .env("GIT_COMMITTER_EMAIL", "wheat@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

/// Runs wheat in `workspace` with extra environment variables.
fn wheat_with(
    workspace: &Path,
    environment: &[(&str, &str)],
    arguments: &[&str],
) -> std::process::Output {
    let mut arguments = arguments.to_vec();
    if arguments.starts_with(&["change", "new"]) {
        arguments.extend(["--message", "test revision"]);
    }
    let config_home = workspace.parent().unwrap().join("wheat-test-config");
    std::fs::create_dir_all(config_home.join("wheat")).unwrap();
    std::fs::write(
        config_home.join("wheat/config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_wheat"))
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .env(
            "WHEAT_STORE_HOME",
            workspace.parent().unwrap().join("stores"),
        )
        .env("XDG_CONFIG_HOME", config_home)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(environment.iter().copied())
        .output()
        .unwrap()
}

fn wheat(workspace: &Path, arguments: &[&str]) -> std::process::Output {
    wheat_with(workspace, &[], arguments)
}

fn succeed(output: std::process::Output) -> std::process::Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn effective_view(workspace: &Path) -> String {
    let document = json(&succeed(wheat(workspace, &["--format=json", "status"])));
    let view: wheat_core::ViewId =
        serde_json::from_value(document["result"]["effective_view"].clone()).unwrap();
    view.to_string()
}

fn store_path(workspace: &Path) -> PathBuf {
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace.join(".wheat/workspace.json")).unwrap())
            .unwrap();
    metadata["store"]["path"].as_str().unwrap().into()
}

fn bare(path: &Path) -> String {
    git(None, &["init", "--bare", path.to_str().unwrap()]);
    path.to_str().unwrap().to_owned()
}

fn change(workspace: &Path, path: &str, value: &str) {
    succeed(wheat(
        workspace,
        &["change", "new", "--path", path, "--value", value],
    ));
}

fn push_main(workspace: &Path, remote: &str) {
    succeed(wheat(
        workspace,
        &["push", remote, "--to", "refs/heads/main"],
    ));
}

fn tag_label(workspace: &Path, tag: &str, view: &str, message: &str) {
    succeed(wheat(
        workspace,
        &[
            "label",
            "create",
            &format!("tags/{tag}"),
            "--view",
            view,
            "-m",
            message,
            "--tagged-at",
            TAGGED_AT,
        ],
    ));
}

/// Pushes `tag` and returns the JSON result.
fn push_tag_with(
    workspace: &Path,
    environment: &[(&str, &str)],
    remote: &str,
    tag: &str,
) -> serde_json::Value {
    json(&succeed(wheat_with(
        workspace,
        environment,
        &["--format=json", "push", remote, "--tag", tag],
    )))["result"]
        .clone()
}

fn push_tag(workspace: &Path, remote: &str, tag: &str) -> serde_json::Value {
    push_tag_with(workspace, &[], remote, tag)
}

fn refused(output: &std::process::Output) -> String {
    assert!(!output.status.success(), "expected a refusal");
    json(output)["error"]["code"].as_str().unwrap().to_owned()
}

#[test]
fn a_tag_names_the_commit_on_main_and_is_identical_from_a_fresh_projection() {
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("repository");
    let first_remote = bare(&root.path().join("first.git"));
    let second_remote = bare(&root.path().join("second.git"));
    succeed(wheat(&repository, &["init", repository.to_str().unwrap()]));
    change(&repository, "one", "first");
    let tagged_view = effective_view(&repository);
    push_main(&repository, &first_remote);
    change(&repository, "two", "second");
    push_main(&repository, &first_remote);
    tag_label(
        &repository,
        "v1",
        &tagged_view,
        "Release 1\n\nThe first one.",
    );

    let pushed = push_tag(&repository, &first_remote, "v1");
    assert_eq!(pushed["destination"], "refs/tags/v1");
    assert_eq!(pushed["up_to_date"], false);
    let tag_oid = pushed["tag_oid"].as_str().unwrap().to_owned();
    let commit_oid = pushed["commit_oid"].as_str().unwrap().to_owned();
    let first = Path::new(&first_remote);
    assert_eq!(git(Some(first), &["rev-parse", "refs/tags/v1"]), tag_oid);
    assert_eq!(git(Some(first), &["cat-file", "-t", "v1"]), "tag");
    assert_eq!(git(Some(first), &["rev-parse", "v1^{commit}"]), commit_oid);
    assert_eq!(git(Some(first), &["rev-parse", "main~1"]), commit_oid);
    assert_eq!(
        git(Some(first), &["cat-file", "tag", "v1"]),
        format!(
            "object {commit_oid}\ntype commit\ntag v1\ntagger Wheat Test <wheat@example.invalid> 1790391600 +0900\n\nRelease 1\n\nThe first one."
        )
    );
    assert_eq!(git(Some(first), &["fsck", "--strict", "--no-dangling"]), "");

    let again = push_tag(&repository, &first_remote, "v1");
    assert_eq!(again["up_to_date"], true);
    assert_eq!(again["tag_oid"], tag_oid.as_str());

    // Another machine: no projection cache, another time zone, and Git configured to
    // sign, convert line endings and use another identity. None of it may leak in.
    std::fs::remove_dir_all(store_path(&repository).join("projections/git.git")).unwrap();
    let hostile_config = root.path().join("hostile.gitconfig");
    std::fs::write(
        &hostile_config,
        "[user]\n\tname = Somebody Else\n\temail = else@example.invalid\n[commit]\n\tgpgsign = true\n[tag]\n\tgpgsign = true\n\tforceSignAnnotated = true\n[core]\n\tautocrlf = true\n",
    )
    .unwrap();
    let hostile = [
        ("TZ", "Pacific/Kiritimati"),
        ("GIT_CONFIG_GLOBAL", hostile_config.to_str().unwrap()),
        ("GIT_AUTHOR_DATE", "2030-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z"),
    ];
    succeed(wheat_with(
        &repository,
        &hostile,
        &["push", &second_remote, "--to", "refs/heads/main"],
    ));
    let second = push_tag_with(&repository, &hostile, &second_remote, "v1");
    assert_eq!(second["tag_oid"], tag_oid.as_str());
    assert_eq!(second["commit_oid"], commit_oid.as_str());
    let second = Path::new(&second_remote);
    assert_eq!(
        git(Some(second), &["rev-parse", "main"]),
        git(Some(first), &["rev-parse", "main"])
    );
    assert_eq!(git(Some(second), &["rev-parse", "refs/tags/v1"]), tag_oid);
}

#[test]
fn a_tag_is_refused_off_every_remote_branch_or_over_a_different_tag() {
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("repository");
    let remote = bare(&root.path().join("remote.git"));
    let remote_path = Path::new(&remote);
    succeed(wheat(&repository, &["init", repository.to_str().unwrap()]));
    change(&repository, "one", "first");
    let pushed_view = effective_view(&repository);
    push_main(&repository, &remote);
    change(&repository, "two", "unpushed");
    let unpushed_view = effective_view(&repository);

    tag_label(&repository, "v2", &unpushed_view, "Not yet");
    let off_branch = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "v2"],
    );
    assert_eq!(refused(&off_branch), "tag_commit_not_on_remote_branch");
    assert_eq!(git(Some(remote_path), &["tag", "--list"]), "");

    // On any branch is enough, and a deleted branch stops counting.
    succeed(wheat(
        &repository,
        &[
            "push",
            &remote,
            "--to",
            "refs/heads/feature",
            "--source",
            &unpushed_view,
        ],
    ));
    tag_label(&repository, "v2-feature", &unpushed_view, "On feature");
    push_tag(&repository, &remote, "v2-feature");
    git(
        Some(remote_path),
        &["update-ref", "-d", "refs/heads/feature"],
    );
    let pruned = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "v2"],
    );
    assert_eq!(refused(&pruned), "tag_commit_not_on_remote_branch");

    // A different tag with the same name is never replaced.
    git(Some(remote_path), &["tag", "v1", "main"]);
    let foreign = git(Some(remote_path), &["rev-parse", "refs/tags/v1"]);
    tag_label(&repository, "v1", &pushed_view, "Release 1");
    let existing = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "v1"],
    );
    assert_eq!(refused(&existing), "tag_exists");
    assert_eq!(
        git(Some(remote_path), &["rev-parse", "refs/tags/v1"]),
        foreign
    );

    succeed(wheat(
        &repository,
        &["label", "create", "tags/plain", "--view", &pushed_view],
    ));
    let plain = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "plain"],
    );
    assert_eq!(refused(&plain), "invalid_input");
    assert!(
        json(&plain)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("wheat label create tags/plain -m")
    );
    let missing = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "missing"],
    );
    assert_eq!(refused(&missing), "invalid_input");

    let both = wheat(
        &repository,
        &[
            "--format=json",
            "push",
            &remote,
            "--tag",
            "v1",
            "--to",
            "refs/heads/main",
        ],
    );
    assert_eq!(refused(&both), "invalid_arguments");
    let invalid = wheat(
        &repository,
        &["--format=json", "push", &remote, "--tag", "bad..name"],
    );
    assert!(!invalid.status.success());
}

#[test]
fn tags_on_imported_history_name_the_original_commits() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let repository = root.path().join("wheat");
    let remote = bare(&root.path().join("remote.git"));
    let remote_path = Path::new(&remote);
    git(None, &["init", "-b", "main", source.to_str().unwrap()]);
    for (file, message) in [("a", "first"), ("b", "second")] {
        std::fs::write(source.join(file), message).unwrap();
        git(Some(&source), &["add", file]);
        git(Some(&source), &["commit", "-m", message]);
    }
    let original = git(Some(&source), &["rev-parse", "HEAD"]);
    git(Some(&source), &["push", &remote, "HEAD:refs/heads/main"]);
    succeed(wheat(
        &repository,
        &[
            "import",
            source.to_str().unwrap(),
            repository.to_str().unwrap(),
            "--branch",
            "refs/heads/main",
        ],
    ));
    let imported_view = effective_view(&repository);
    change(&repository, "native", "from wheat");
    let native_view = effective_view(&repository);
    push_main(&repository, &remote);
    assert_eq!(git(Some(remote_path), &["rev-parse", "main^"]), original);

    tag_label(&repository, "imported", &imported_view, "Imported");
    tag_label(&repository, "native", &native_view, "Native");
    let imported = push_tag(&repository, &remote, "imported");
    assert_eq!(imported["commit_oid"], original.as_str());
    let native = push_tag(&repository, &remote, "native");
    assert_eq!(
        native["commit_oid"],
        git(Some(remote_path), &["rev-parse", "main"]).as_str()
    );
    assert_eq!(
        git(Some(remote_path), &["rev-parse", "imported^{commit}"]),
        original
    );
}
