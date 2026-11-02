#![cfg(target_os = "linux")]

//! A directory too wide for one Wheat tree node goes through Git both ways: pulled
//! from Git, it is stored in chunks; pushed back, it is one Git tree again.

use crate::git_team;

use git_team::{Team, git};

const FILES: usize = 300;

#[test]
fn a_wide_directory_round_trips_through_git() {
    let team = Team::new();

    // Bob adds a directory wider than one Wheat tree node.
    git(Some(&team.bob), &["pull", "--ff-only", "origin", "main"]);
    std::fs::create_dir(team.bob.join("wide")).unwrap();
    for index in 0..FILES {
        std::fs::write(
            team.bob.join(format!("wide/f{index}.txt")),
            format!("file {index}\n"),
        )
        .unwrap();
    }
    git(Some(&team.bob), &["add", "wide"]);
    git(Some(&team.bob), &["commit", "-m", "bob: add wide"]);
    git(Some(&team.bob), &["push", "origin", "main"]);
    let bob_tree = git(Some(&team.bob), &["rev-parse", "HEAD:wide"]);

    team.pull();
    assert_eq!(team.read("wide/f7.txt"), "file 7\n");
    team.wheat_ok(&["validate"]);

    // Wheat changes one file of it and pushes.
    team.wheat_change("wheat: change f7", "wide/f7.txt", "changed\n");
    team.push();
    git(Some(&team.remote), &["fsck", "--strict"]);
    let listing = git(Some(&team.remote), &["ls-tree", "refs/heads/main:wide"]);
    assert_eq!(listing.lines().count(), FILES);
    // Only that file differs from the tree Bob's Git wrote.
    let changed = git(
        Some(&team.remote),
        &[
            "diff-tree",
            "--name-only",
            &bob_tree,
            "refs/heads/main:wide",
        ],
    );
    assert_eq!(changed, "f7.txt");

    // Bob changes another file on top; Wheat pulls it.
    git(Some(&team.bob), &["pull", "--ff-only", "origin", "main"]);
    assert_eq!(
        std::fs::read_to_string(team.bob.join("wide/f7.txt")).unwrap(),
        "changed\n"
    );
    team.bob_commits("bob: change f9", "wide/f9.txt", "bob\n");
    team.pull();
    assert_eq!(team.read("wide/f9.txt"), "bob\n");
    assert_eq!(team.read("wide/f7.txt"), "changed\n");
    team.assert_clean();
    team.wheat_ok(&["validate"]);
}
