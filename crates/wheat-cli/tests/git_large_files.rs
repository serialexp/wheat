#![cfg(target_os = "linux")]

//! Files past the chunk threshold through the Git bridge: pushed from Wheat, written
//! by Git users and pulled, and imported fresh. Each is streamed rather than held.

use crate::git_team;

use git_team::{Team, git};

/// Longer than both the chunk threshold and the Git importer's streaming bound.
const LONG: usize = 17 * 1024 * 1024;

fn long_binary(seed: u8) -> Vec<u8> {
    (0..LONG).map(|index| (index % 251) as u8 ^ seed).collect()
}

fn long_crlf_text() -> Vec<u8> {
    let mut text = Vec::with_capacity(LONG + 16);
    let mut line = 0_usize;
    while text.len() <= LONG {
        text.extend_from_slice(format!("line {line:08}\r\n").as_bytes());
        line += 1;
    }
    text
}

#[test]
fn long_files_round_trip_through_git() {
    let team = Team::new();

    // Wheat pushes a long file, and Git holds exactly its bytes.
    let pushed = long_binary(0);
    std::fs::write(team.workspace.join("long.bin"), &pushed).unwrap();
    team.wheat_ok(&["change", "new", "--message", "wheat: long file"]);
    team.wheat_ok(&["add", "long.bin"]);
    team.push();
    let expected = git(
        None,
        &[
            "hash-object",
            team.workspace.join("long.bin").to_str().unwrap(),
        ],
    );
    assert_eq!(
        git(
            Some(&team.remote),
            &["rev-parse", "refs/heads/main:long.bin"]
        ),
        expected
    );

    // Bob replaces it, and adds long text with CRLF line endings.
    git(Some(&team.bob), &["pull", "--ff-only", "origin", "main"]);
    assert_eq!(std::fs::read(team.bob.join("long.bin")).unwrap(), pushed);
    let replaced = long_binary(0x5a);
    let text = long_crlf_text();
    std::fs::write(team.bob.join("long.bin"), &replaced).unwrap();
    std::fs::write(team.bob.join("long.txt"), &text).unwrap();
    git(Some(&team.bob), &["add", "long.bin", "long.txt"]);
    git(Some(&team.bob), &["commit", "-m", "bob: long files"]);
    git(Some(&team.bob), &["push", "origin", "main"]);

    // Pulling brings both in; the text is canonicalized as any other text is.
    team.pull();
    let canonical = text
        .iter()
        .copied()
        .filter(|byte| *byte != b'\r')
        .collect::<Vec<_>>();
    assert_eq!(
        std::fs::read(team.workspace.join("long.bin")).unwrap(),
        replaced
    );
    assert_eq!(
        std::fs::read(team.workspace.join("long.txt")).unwrap(),
        canonical
    );
    team.assert_clean();
    team.wheat_ok(&["validate"]);

    // A fresh import of the branch stages the same files.
    let imported = team.root.path().join("imported");
    team.wheat_ok(&[
        "import",
        team.remote.to_str().unwrap(),
        imported.to_str().unwrap(),
        "--branch",
        "refs/heads/main",
    ]);
    assert_eq!(std::fs::read(imported.join("long.bin")).unwrap(), replaced);
    assert_eq!(std::fs::read(imported.join("long.txt")).unwrap(), canonical);
    team.wheat_ok_in(&imported, &["validate"]);
}
