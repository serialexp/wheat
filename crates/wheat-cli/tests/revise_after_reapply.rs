#![cfg(target_os = "linux")]
//! A change must stay revisable after an ancestor revision reapplies it.
//!
//! Revising an ancestor reapplies every descendant onto the ancestor's new result, so a
//! descendant's current application target is no longer the view its latest revision was
//! authored against. The next `add` to that descendant must author against the target it is
//! actually applied to, which is the tree the workspace shows.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let config = dir.path().join("config").join("wheat");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
        )
        .unwrap();
        let workspace = Self { _dir: dir, root };
        workspace.ok(&["init", workspace.root.to_str().unwrap()]);
        workspace
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let home = self.root.parent().unwrap();
        Command::new(env!("CARGO_BIN_EXE_wheat"))
            .arg("--workspace")
            .arg(&self.root)
            .args(arguments)
            .env("WHEAT_STORE_HOME", home.join("stores"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .output()
            .expect("wheat command runs")
    }

    fn ok(&self, arguments: &[&str]) -> Output {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "command {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.root.join(path), contents).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }
}

fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| {
            panic!(
                "no {name:?} line in {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
        .to_owned()
}

#[test]
fn descendant_is_revisable_after_ancestor_revision_reapplies_it() {
    let workspace = Workspace::new();

    // Ancestor A owns `f` and introduces `shared.txt`.
    let ancestor = field(&workspace.ok(&["change", "new", "-m", "A"]), "change");
    workspace.write("f", "one\n");
    workspace.write("shared.txt", "top\nmiddle\nbottom\n");
    workspace.ok(&["add", "-m", "A1", "f", "shared.txt"]);

    // Descendant D adds `other` and makes a guarded text edit in `shared.txt`.
    let descendant = field(
        &workspace.ok(&[
            "change", "new", "-m", "D", "--path", "other", "--value", "x",
        ]),
        "change",
    );
    workspace.ok(&["materialize"]);
    assert_eq!(workspace.read("other"), "x");
    workspace.write("shared.txt", "top\nmiddle\nbottom revised by D\n");
    workspace.ok(&["add", "-m", "D2", "shared.txt"]);

    // Revising A reapplies D onto A's new result.
    workspace.write("f", "two\n");
    let revised = workspace.ok(&["add", "-m", "A2", "--change", &ancestor, "f"]);
    assert!(
        String::from_utf8_lossy(&revised.stdout).contains(&format!("reapplied {descendant}")),
        "revising A must reapply D: {}",
        String::from_utf8_lossy(&revised.stdout)
    );

    // D must still be revisable, both for a whole-file capture and a guarded text edit.
    workspace.write("other", "y\n");
    workspace.write("shared.txt", "top\nmiddle\nbottom revised by D again\n");
    workspace.ok(&["add", "-m", "D3", "other", "shared.txt"]);

    // The workspace is exactly D's new result on top of A's revision, and the repository,
    // including the replay of every revision against the view it was authored against,
    // still validates.
    let status = workspace.ok(&["status"]);
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("tracked changes: 0"),
        "workspace should be clean after the add: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    workspace.ok(&["validate"]);
    workspace.ok(&["materialize"]);
    assert_eq!(workspace.read("f"), "two\n");
    assert_eq!(workspace.read("other"), "y\n");
    assert_eq!(
        workspace.read("shared.txt"),
        "top\nmiddle\nbottom revised by D again\n"
    );
}
