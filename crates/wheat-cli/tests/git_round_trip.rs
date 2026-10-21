#![cfg(target_os = "linux")]

//! The Git round trip a team sharing one Git branch goes through: import it, push
//! Wheat changes to it, pull commits other people pushed, and push again on top of
//! them without force.

mod git_team;

use git_team::{Team, git};

#[test]
fn pushed_and_pulled_git_history_round_trips() {
    let team = Team::new();
    assert_eq!(team.change_titles(), ["first", "second"]);

    // Wheat pushes a change to the shared branch.
    team.wheat_change("wheat: three", "a.txt", "one\ntwo\nthree\n");
    team.push();
    assert_eq!(team.remote_history(), ["wheat: three", "second", "first"]);
    let pushed_three = team.remote_main();

    // Bob builds on it in Git while Wheat makes a change it hasn't pushed yet.
    let bob = team.bob_commits("bob: add b", "b.txt", "bob\n");
    assert_eq!(git(Some(&team.bob), &["rev-parse", "HEAD^"]), pushed_three);
    team.wheat_change("wheat: add c", "c.txt", "wheat c\n");

    // Pulling brings in Bob's commit, recognises Wheat's own pushed commit as the
    // change it came from, and puts the unpushed change on top.
    team.pull();
    assert_eq!(
        team.change_titles(),
        [
            "bob: add b",
            "first",
            "second",
            "wheat: add c",
            "wheat: three"
        ]
    );
    assert_eq!(team.read("a.txt"), "one\ntwo\nthree\n");
    assert_eq!(team.read("b.txt"), "bob\n");
    assert_eq!(team.read("c.txt"), "wheat c\n");
    team.assert_clean();
    team.wheat_ok(&["validate"]);

    // The next push fast-forwards: Bob's commit stays exactly as he pushed it.
    team.push();
    assert_eq!(
        team.remote_history(),
        [
            "wheat: add c",
            "bob: add b",
            "wheat: three",
            "second",
            "first"
        ]
    );
    assert_eq!(
        git(Some(&team.remote), &["rev-parse", "refs/heads/main^"]),
        bob
    );
    assert_eq!(
        git(Some(&team.remote), &["rev-parse", "refs/heads/main^^"]),
        pushed_three
    );
    git(Some(&team.bob), &["pull", "--ff-only", "origin", "main"]);
    assert_eq!(
        std::fs::read_to_string(team.bob.join("c.txt")).unwrap(),
        "wheat c\n"
    );

    // Pulling what Wheat just pushed changes nothing.
    // The push recorded the commits it wrote, which advanced the repository past the
    // workspace's materialization; the up-to-date pull leaves the workspace alone.
    team.pull();
    assert_eq!(team.change_titles().len(), 5);
    assert_eq!(team.assert_no_work()["authority_advanced"], true);
    team.push();
    assert_eq!(team.remote_history().len(), 5);
}
