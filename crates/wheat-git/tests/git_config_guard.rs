//! Tests must see git's built-in defaults, never the developer's own
//! `~/.gitconfig` or the machine's system config. `.cargo/config.toml` arranges
//! that for everything Cargo and nextest start (`GIT_CONFIG_GLOBAL=/dev/null`,
//! `GIT_CONFIG_NOSYSTEM=1`); this guard fails when a test run gets around it, for
//! example a test binary started by hand, so a personal setting like
//! `init.defaultBranch = main` or `commit.gpgsign = true` can't quietly decide
//! whether the suite passes.

use std::process::Command;

#[test]
fn tests_see_no_git_config() {
    // Outside any repository, `git config --list` shows only the global and system
    // files (and `-c` overrides, of which there are none).
    let outside_any_repo = tempfile::tempdir().unwrap();
    let output = Command::new("git")
        .args(["config", "--list", "--show-origin"])
        .current_dir(outside_any_repo.path())
        .output()
        .expect("git is installed");
    let visible = String::from_utf8_lossy(&output.stdout);
    assert!(
        visible.trim().is_empty(),
        "tests can see git config from this machine, so their results depend on \
         it. Run them through `cargo nextest run` / `cargo test` / `just test`, \
         which apply .cargo/config.toml's [env] section \
         (GIT_CONFIG_GLOBAL=/dev/null, GIT_CONFIG_NOSYSTEM=1). Visible:\n{visible}"
    );
}
