#![cfg(target_os = "linux")]

//! `wheat pull <git-url>`: what it appends, how it moves local work, and what it
//! refuses.

mod git_team;

use std::{fmt::Write as _, io::Write as _};

use git_team::{Team, git};

/// The newest `wheat log` entry titled `title`.
fn log_entry(team: &Team, title: &str) -> serde_json::Value {
    team.wheat_json(&["log"])["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["change"]["title"] == title)
        .unwrap_or_else(|| panic!("no change titled {title}"))
        .clone()
}

#[test]
fn an_up_to_date_branch_is_left_alone() {
    let team = Team::new();
    let before = team.wheat_json(&["status"]);

    let pulled = team.pull();

    assert_eq!(pulled["pulled"], false, "{pulled:#}");
    let after = team.wheat_json(&["status"]);
    assert_eq!(
        after["current_head_generation"], before["current_head_generation"],
        "an up-to-date pull publishes nothing"
    );
}

#[test]
fn new_commits_are_appended_above_imported_history() {
    let team = Team::new();
    team.bob_commits("bob: add b", "b.txt", "bob\n");
    let tip = team.bob_commits("bob: edit a", "a.txt", "one\ntwo\nbob\n");

    let pulled = team.pull();

    assert_eq!(pulled["pulled"], true, "{pulled:#}");
    assert_eq!(pulled["remote_tip"], tip);
    assert_eq!(pulled["appended"].as_array().unwrap().len(), 2);
    assert_eq!(pulled["rebased"], serde_json::json!([]));
    assert_eq!(
        team.change_titles(),
        ["bob: add b", "bob: edit a", "first", "second"]
    );
    assert_eq!(team.read("a.txt"), "one\ntwo\nbob\n");
    assert_eq!(team.read("b.txt"), "bob\n");
    team.assert_clean();
    team.wheat_ok(&["validate"]);

    // The pulled commits are recognised as imported on the next pull.
    assert_eq!(team.pull()["pulled"], false);
    assert_eq!(team.change_titles().len(), 4);
}

#[test]
fn local_work_moves_above_the_pulled_commits_like_a_git_rebase() {
    let team = Team::new();
    std::fs::write(team.workspace.join("c.txt"), "wheat c\n").unwrap();
    team.wheat_ok(&["change", "new", "--message", "wheat: add c"]);
    team.wheat_ok(&[
        "add",
        "c.txt",
        "--authored-at",
        "2020-01-01T00:00:00Z",
        "--committed-at",
        "2020-01-01T00:00:00Z",
    ]);
    let before = log_entry(&team, "wheat: add c");
    team.bob_commits("bob: add b", "b.txt", "bob\n");

    let pulled = team.pull();

    let bob = log_entry(&team, "bob: add b");
    let after = log_entry(&team, "wheat: add c");
    assert_eq!(pulled["rebased"], serde_json::json!([after["change_id"]]));
    assert_eq!(pulled["conflicted"], serde_json::json!([]));
    assert_eq!(after["change_id"], before["change_id"]);
    assert_eq!(after["predecessor"], before["revision_id"]);
    assert_eq!(after["dependencies"], serde_json::json!([bob["change_id"]]));
    assert_eq!(after["revision"]["message"], before["revision"]["message"]);
    assert_eq!(after["revision"]["author"], before["revision"]["author"]);
    assert_eq!(
        after["revision"]["authored_at"],
        before["revision"]["authored_at"]
    );
    assert_ne!(
        after["revision"]["committed_at"], before["revision"]["committed_at"],
        "the puller commits the moved revision now"
    );
    assert_eq!(team.read("b.txt"), "bob\n");
    assert_eq!(team.read("c.txt"), "wheat c\n");
    team.assert_clean();
    team.wheat_ok(&["validate"]);
}

#[test]
fn local_work_that_no_longer_applies_is_a_durable_conflict() {
    let team = Team::new();
    team.wheat_change("wheat: two", "a.txt", "one\nTWO\n");
    team.bob_commits("bob: two", "a.txt", "one\ndeux\n");

    let pulled = team.pull();

    let wheat = log_entry(&team, "wheat: two");
    assert_eq!(
        pulled["conflicted"],
        serde_json::json!([wheat["change_id"]]),
        "{pulled:#}"
    );
    let conflicts = team.wheat_json(&["conflicts"]);
    assert!(
        conflicts
            .to_string()
            .contains(wheat["change_id"].as_str().unwrap()),
        "{conflicts:#}"
    );
    assert_eq!(team.read("a.txt"), "one\ndeux\n");
    team.wheat_ok(&["validate"]);
}

#[test]
fn a_merge_is_one_change_along_first_parents() {
    let team = Team::new();
    git(Some(&team.bob), &["checkout", "-b", "side"]);
    std::fs::write(team.bob.join("s.txt"), "side\n").unwrap();
    git(Some(&team.bob), &["add", "s.txt"]);
    git(Some(&team.bob), &["commit", "-m", "side: add s"]);
    git(Some(&team.bob), &["checkout", "main"]);
    std::fs::write(team.bob.join("m.txt"), "main\n").unwrap();
    git(Some(&team.bob), &["add", "m.txt"]);
    git(Some(&team.bob), &["commit", "-m", "bob: add m"]);
    git(
        Some(&team.bob),
        &["merge", "--no-ff", "-m", "bob: merge side", "side"],
    );
    git(Some(&team.bob), &["push", "origin", "main"]);

    let pulled = team.pull();

    assert_eq!(pulled["appended"].as_array().unwrap().len(), 2);
    assert_eq!(
        team.change_titles(),
        ["bob: add m", "bob: merge side", "first", "second"]
    );
    assert_eq!(team.read("s.txt"), "side\n");
    assert_eq!(team.read("m.txt"), "main\n");
    team.assert_clean();
    team.wheat_ok(&["validate"]);
}

#[test]
fn from_pulls_another_branch() {
    let team = Team::new();
    git(Some(&team.bob), &["checkout", "-b", "feature"]);
    std::fs::write(team.bob.join("f.txt"), "feature\n").unwrap();
    git(Some(&team.bob), &["add", "f.txt"]);
    git(Some(&team.bob), &["commit", "-m", "bob: feature"]);
    git(Some(&team.bob), &["push", "origin", "feature"]);

    assert_eq!(team.pull()["pulled"], false, "main has nothing new");
    let pulled = team.wheat_json(&["pull", team.remote_url(), "--from", "refs/heads/feature"]);

    assert_eq!(pulled["branch"], "refs/heads/feature");
    assert_eq!(pulled["pulled"], true);
    assert_eq!(team.read("f.txt"), "feature\n");
}

/// The commit refs of a projection pushed before they existed are rebuilt from its
/// application refs.
#[test]
fn pushed_commits_are_recognised_in_a_projection_without_commit_refs() {
    let team = Team::new();
    team.wheat_change("wheat: three", "a.txt", "one\ntwo\nthree\n");
    team.push();
    let projection = team.projection();
    let commit_refs = || {
        git(
            Some(&projection),
            &["for-each-ref", "--format=%(refname)", "refs/wheat/commits/"],
        )
    };
    let written = commit_refs();
    assert!(!written.is_empty());
    for name in written.lines() {
        git(Some(&projection), &["update-ref", "-d", name]);
    }
    assert_eq!(commit_refs(), "");
    team.bob_commits("bob: add b", "b.txt", "bob\n");

    team.pull();

    assert_eq!(
        team.change_titles(),
        ["bob: add b", "first", "second", "wheat: three"]
    );
    assert_eq!(commit_refs(), written);
}

#[test]
fn a_pushed_commit_revised_since_is_refused() {
    let team = Team::new();
    team.wheat_change("wheat: three", "a.txt", "one\ntwo\nthree\n");
    team.push();
    std::fs::write(team.workspace.join("a.txt"), "one\ntwo\nTHREE\n").unwrap();
    team.wheat_ok(&["add", "a.txt"]);
    team.bob_commits("bob: add b", "b.txt", "bob\n");

    let (code, error) = team.wheat_error(&["pull", team.remote_url()]);

    assert_eq!(code, Some(3), "{error:#}");
    assert_eq!(error["code"], "git_history_off_path");
    assert_eq!(team.change_titles(), ["first", "second", "wheat: three"]);
}

#[test]
fn a_commit_ref_naming_a_different_tree_is_refused() {
    let team = Team::new();
    team.wheat_change("wheat: three", "a.txt", "one\ntwo\nthree\n");
    team.push();
    let projection = team.projection();
    let pushed = team.remote_main();
    let written = git(
        Some(&projection),
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/wheat/commits/{pushed}/"),
        ],
    );
    let application = written.rsplit('/').next().unwrap().to_owned();
    let bob = team.bob_commits("bob: add b", "b.txt", "bob\n");
    git(Some(&projection), &["fetch", team.remote_url(), "main"]);
    git(
        Some(&projection),
        &[
            "update-ref",
            &format!("refs/wheat/commits/{bob}/{application}"),
            &bob,
        ],
    );

    let (code, error) = team.wheat_error(&["pull", team.remote_url()]);

    assert_eq!(code, Some(6), "{error:#}");
    assert_eq!(error["code"], "corrupt_git_projection");
}

#[test]
fn a_pull_of_more_commits_than_the_limit_is_refused() {
    let team = Team::new();
    git(Some(&team.bob), &["pull", "--ff-only", "origin", "main"]);
    let mut stream = String::new();
    for number in 0..=10_000 {
        let message = format!("empty {number}");
        let parent = if number == 0 {
            "refs/heads/main^0"
        } else {
            ":1"
        };
        write!(
            stream,
            "commit refs/heads/main\nmark :1\ncommitter Bob Git <bob@example.invalid> \
             1767323045 +0000\ndata {}\n{message}\nfrom {parent}\n\n",
            message.len()
        )
        .unwrap();
    }
    let mut import = std::process::Command::new("git")
        .args([
            "-C",
            team.bob.to_str().unwrap(),
            "fast-import",
            "--quiet",
            "--force",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    import
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(import.wait().unwrap().success());
    git(Some(&team.bob), &["push", "origin", "main"]);

    let (code, error) = team.wheat_error(&["pull", team.remote_url()]);

    assert_eq!(code, Some(5), "{error:#}");
    assert_eq!(error["code"], "limit_exceeded");
    assert_eq!(team.change_titles(), ["first", "second"]);
}

#[test]
fn a_push_behind_the_remote_says_to_pull_first() {
    let team = Team::new();
    team.bob_commits("bob: add b", "b.txt", "bob\n");
    team.wheat_change("wheat: add c", "c.txt", "c\n");

    let (code, error) = team.wheat_error(&["push", team.remote_url(), "--to", "refs/heads/main"]);

    assert_eq!(code, Some(3), "{error:#}");
    assert_eq!(error["code"], "git_remote_diverged");
    assert_eq!(error["retryable"], false);
    assert!(
        error["message"].as_str().unwrap().contains("wheat pull"),
        "{error:#}"
    );
    team.pull();
    team.push();
    assert_eq!(
        team.remote_history(),
        ["wheat: add c", "bob: add b", "second", "first"]
    );
}

#[test]
fn a_dirty_workspace_is_refused() {
    let team = Team::new();
    team.bob_commits("bob: add b", "b.txt", "bob\n");
    std::fs::write(team.workspace.join("a.txt"), "uncaptured\n").unwrap();

    let (code, error) = team.wheat_error(&["pull", team.remote_url()]);

    assert_eq!(code, Some(2), "{error:#}");
    assert_eq!(team.change_titles(), ["first", "second"]);
    assert_eq!(team.read("a.txt"), "uncaptured\n");
}

#[test]
fn a_label_not_imported_from_git_needs_from() {
    let team = Team::new();
    team.wheat_ok(&["label", "create", "native"]);
    team.wheat_ok(&["switch", "native"]);
    team.bob_commits("bob: add b", "b.txt", "bob\n");

    let (code, error) = team.wheat_error(&["pull", team.remote_url()]);
    assert_eq!(code, Some(2), "{error:#}");
    assert!(
        error["message"].as_str().unwrap().contains("--from"),
        "{error:#}"
    );

    let pulled = team.wheat_json(&["pull", team.remote_url(), "--from", "refs/heads/main"]);
    assert_eq!(pulled["label"], "native");
    assert_eq!(team.read("b.txt"), "bob\n");
}

#[test]
fn an_absent_branch_is_refused() {
    let team = Team::new();

    let (code, error) =
        team.wheat_error(&["pull", team.remote_url(), "--from", "refs/heads/absent"]);

    assert_eq!(code, Some(2), "{error:#}");
    assert_eq!(error["code"], "remote_branch_absent");
}

#[test]
fn unrelated_history_is_refused() {
    let team = Team::new();
    let unrelated = team.root.path().join("unrelated");
    git(None, &["init", "-b", "main", unrelated.to_str().unwrap()]);
    std::fs::write(unrelated.join("u.txt"), "unrelated\n").unwrap();
    git(Some(&unrelated), &["add", "u.txt"]);
    git(Some(&unrelated), &["commit", "-m", "unrelated"]);

    let (code, error) = team.wheat_error(&["pull", unrelated.to_str().unwrap()]);

    assert_eq!(code, Some(2), "{error:#}");
    assert_eq!(error["code"], "disconnected_history");
    assert_eq!(team.change_titles(), ["first", "second"]);
}

#[test]
fn a_wheat_url_pulls_natively() {
    let team = Team::new();

    // The native pull needs the remote this store was cloned from; an imported store
    // has none.
    let (_, error) = team.wheat_error(&["pull", "wheat://127.0.0.1:1/owner/repo"]);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("no remote.json found"),
        "{error:#}"
    );

    let (code, error) = team.wheat_error(&[
        "pull",
        "wheat://127.0.0.1:1/owner/repo",
        "--from",
        "refs/heads/main",
    ]);
    assert_eq!(code, Some(2), "{error:#}");
    assert_eq!(error["code"], "invalid_input");
}
