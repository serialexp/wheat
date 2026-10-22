#![cfg(target_os = "linux")]

//! The desktop client's writes: adding files to a change, creating and selecting
//! changes, and restoring files, called as a library against a workspace the `wheat`
//! command initialized.

use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use wheat_client::{
    authoring::{self, AddPaths, AddRequest, NewChange},
    authoring_metadata::MetadataArgs,
    dot_git::DotGitGate,
    history,
};
use wheat_core::{ChangeId, RevisionEdits};

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let workspace = Self { _dir: dir, root };
        workspace.wheat(&["init"]);
        workspace.wheat(&["config", "set", "actor.name", "Test"]);
        workspace.wheat(&["config", "set", "actor.email", "test@example.invalid"]);
        workspace
    }

    fn wheat(&self, args: &[&str]) {
        let home = self.root.parent().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
            .current_dir(&self.root)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("WHEAT_STORE_HOME", home.join("stores"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.root.join(path), contents).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    fn create(&self, message: &str) -> ChangeId {
        authoring::create_change(
            self.path(),
            None,
            NewChange {
                dependencies: Default::default(),
                independent: false,
                metadata: MetadataArgs {
                    message: message.to_owned(),
                    description: None,
                    created_at: None,
                    authored_at: None,
                    committed_at: None,
                },
            },
            |_| Ok(RevisionEdits::default()),
        )
        .unwrap()
        .change
    }

    fn add(
        &self,
        paths: AddPaths,
        change: Option<ChangeId>,
        message: &str,
        confirm: impl FnOnce(ChangeId, &[String]) -> anyhow::Result<()>,
    ) -> anyhow::Result<authoring::AddOutcome> {
        authoring::add(
            self.path(),
            None,
            AddRequest {
                paths,
                change,
                predecessor: None,
                guards: Default::default(),
                message: message.to_owned(),
                authored_at: None,
                committed_at: None,
            },
            confirm,
        )
    }

    fn current(&self, change: ChangeId) -> history::HistoryEntry {
        history::read(self.path(), None)
            .unwrap()
            .into_iter()
            .find(|entry| entry.change == change)
            .unwrap()
    }
}

fn never(_: ChangeId, moves: &[String]) -> anyhow::Result<()> {
    panic!("nothing should move: {moves:?}")
}

fn listed(paths: &[&str]) -> AddPaths {
    AddPaths::Listed(paths.iter().map(|path| (*path).to_owned()).collect())
}

#[test]
fn a_new_change_is_selected_and_takes_the_next_add() {
    let workspace = Workspace::new();
    let change = workspace.create("first change");
    workspace.write("file", "one\n");
    let added = workspace
        .add(listed(&["file"]), None, "add file", never)
        .unwrap();
    assert_eq!(added.change, change);
    assert_eq!(added.title, "first change");
    assert_eq!(added.paths, 1);
    assert_eq!(added.selected, Some(change));
    assert!(added.moves.is_empty() && added.moved.is_empty());
    assert!(!added.materialize_required);
    assert!(added.diverged.is_empty());
    let entry = workspace.current(change);
    assert_eq!(entry.revision, added.revision);
    assert_eq!(entry.message, "add file");
}

#[test]
fn adding_modified_paths_takes_every_changed_tracked_file_and_refuses_a_clean_workspace() {
    let workspace = Workspace::new();
    workspace.create("change");
    workspace.write("a", "a\n");
    workspace.write("b", "b\n");
    workspace
        .add(listed(&["a", "b"]), None, "add both", never)
        .unwrap();
    let clean = workspace
        .add(AddPaths::Modified, None, "nothing", never)
        .err()
        .unwrap();
    assert!(
        clean.to_string().contains("workspace is clean"),
        "{clean:#}"
    );

    workspace.write("b", "changed\n");
    let added = workspace
        .add(AddPaths::Modified, None, "change b", never)
        .unwrap();
    assert_eq!(added.paths, 1);
}

#[test]
fn selecting_a_change_returns_its_title_and_redirects_adds() {
    let workspace = Workspace::new();
    let first = workspace.create("first");
    workspace.write("a", "a\n");
    workspace.add(listed(&["a"]), None, "a", never).unwrap();
    let second = workspace.create("second");
    assert_eq!(
        authoring::select_change(workspace.path(), None, first).unwrap(),
        "first"
    );
    workspace.write("a", "a2\n");
    let added = workspace.add(listed(&["a"]), None, "a2", never).unwrap();
    assert_eq!(added.change, first);
    assert_eq!(added.selected, Some(first));
    assert_ne!(workspace.current(second).revision, added.revision);
    assert!(authoring::select_change(workspace.path(), None, ChangeId::new()).is_err());
}

#[test]
fn restore_discards_uncaptured_edits() {
    let workspace = Workspace::new();
    workspace.create("change");
    workspace.write("file", "kept\n");
    workspace
        .add(listed(&["file"]), None, "add file", never)
        .unwrap();
    workspace.write("file", "scribbled\n");
    let restored = authoring::restore(
        workspace.path(),
        None,
        &["file".to_owned()],
        DotGitGate::Allow,
    )
    .unwrap();
    assert_eq!(restored, 1);
    assert_eq!(workspace.read("file"), "kept\n");
}

/// Change B creates `file`; change C rewrites its tenth line. Rewriting that line again
/// into B moves C's edit into B, which the caller confirms first.
#[test]
fn moving_a_later_changes_edit_waits_for_confirmation() {
    let workspace = Workspace::new();
    let base: String = (1..=20).map(|n| format!("l{n}\n")).collect();
    let b = workspace.create("B");
    workspace.write("file", &base);
    workspace.add(listed(&["file"]), None, "B1", never).unwrap();
    let c = workspace.create("C");
    workspace.write("file", &base.replace("l10\n", "C1\nC2\n"));
    workspace.add(listed(&["file"]), None, "C1", never).unwrap();
    let before = (workspace.current(b).revision, workspace.current(c).revision);

    workspace.write("file", &base.replace("l10\n", "B2\n"));
    let asked = Cell::new(false);
    let refused = workspace.add(listed(&["file"]), Some(b), "B2", |change, moves| {
        asked.set(true);
        assert_eq!(change, b);
        assert!(!moves.is_empty());
        anyhow::bail!("not now")
    });
    assert!(asked.get());
    assert!(refused.err().unwrap().to_string().contains("not now"));
    assert_eq!(
        (workspace.current(b).revision, workspace.current(c).revision),
        before,
        "a refused move publishes nothing"
    );

    let added = workspace
        .add(listed(&["file"]), Some(b), "B2", |_, _| Ok(()))
        .unwrap();
    assert_eq!(added.change, b);
    assert_eq!(added.selected, Some(c), "the selection stays on C");
    assert!(!added.moves.is_empty());
    let moved = added
        .moved
        .iter()
        .find(|moved| moved.path == "file")
        .unwrap();
    assert_eq!(moved.from, c);
    assert_eq!(moved.revision, workspace.current(c).revision);
}
