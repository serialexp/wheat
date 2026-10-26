#![cfg(target_os = "linux")]

//! `wheat label move`: forward moves, refusals, `--force`, the workspace rebind and
//! its recovery from an interruption on either side of the head's CAS.

mod remote_support;

use std::path::PathBuf;
use std::process::Command;

use remote_support::{fails, ok, ok_json, workspace_store, write_config};
use wheat_client::workspace_metadata::WorkspaceMetadata;

struct Local {
    _root: tempfile::TempDir,
    config: PathBuf,
    stores: PathBuf,
    workspace: PathBuf,
}

impl Local {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        write_config(&config);
        let stores = root.path().join("stores");
        let workspace = root.path().join("workspace");
        ok(
            root.path(),
            &stores,
            &config,
            &["init", workspace.to_str().unwrap()],
        );
        Self {
            _root: root,
            config,
            stores,
            workspace,
        }
    }

    fn ok_json(&self, args: &[&str]) -> serde_json::Value {
        ok_json(&self.workspace, &self.stores, &self.config, args)
    }

    fn fails(&self, args: &[&str]) -> String {
        let args: Vec<&str> = ["--format", "json"].iter().chain(args).copied().collect();
        fails(&self.workspace, &self.stores, &self.config, &args)
    }

    fn metadata(&self) -> WorkspaceMetadata {
        WorkspaceMetadata::read(&self.workspace).unwrap()
    }

    /// A new change writing `path`, which the workspace then selects as a frontier.
    fn change(&self, path: &str) -> String {
        self.ok_json(&[
            "change",
            "new",
            "--path",
            path,
            "--value",
            path,
            "--message",
            path,
        ]);
        self.metadata()
            .selected_frontier
            .expect("a new change is a frontier")
            .to_string()
    }

    fn label_view(&self, name: &str) -> String {
        let labels = self.ok_json(&["labels"]);
        labels["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|label| label["name"] == name)
            .map(|label| label["view"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| panic!("no label {name}: {labels}"))
    }

    /// `label move` stopped at `checkpoint`, as if the process died there.
    fn interrupted_move(&self, checkpoint: &str) {
        let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
            .current_dir(&self.workspace)
            .args(["label", "move", "main"])
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_AFTER", checkpoint)
            .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "exit")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn view(value: &str) -> wheat_core::ViewId {
    value.parse().unwrap()
}

#[test]
fn a_forward_move_to_the_workspace_view_rebinds_the_workspace() {
    let local = Local::new();
    let base = local.label_view("main");
    let frontier = local.change("a.txt");

    let moved = local.ok_json(&["label", "move", "main"]);
    assert_eq!(moved["moved"], true, "{moved}");
    assert_eq!(moved["rebound"], true, "{moved}");
    assert_eq!(moved["from"], base.as_str(), "{moved}");
    assert_eq!(moved["label"]["view"], frontier.as_str(), "{moved}");
    let metadata = local.metadata();
    assert_eq!(metadata.basis, view(&frontier));
    assert_eq!(metadata.observed_label_binding, view(&frontier));
    assert_eq!(metadata.selected_frontier, None);
    assert_eq!(metadata.pending_repository_operation, None);
    assert_eq!(local.label_view("main"), frontier);
    local.ok_json(&["materialize"]);
    let frontiers = local.ok_json(&["frontier", "list"]);
    assert_eq!(frontiers["frontiers"], serde_json::json!([]), "{frontiers}");

    // Moving it again to where it is changes nothing.
    let again = local.ok_json(&["label", "move", "main"]);
    assert_eq!(again["moved"], false, "{again}");
}

#[test]
fn a_backward_move_needs_force_and_leaves_the_workspace_bound_where_it_was() {
    let local = Local::new();
    let base = local.label_view("main");
    let frontier = local.change("a.txt");
    local.ok_json(&["label", "move", "main"]);

    let refused = local.fails(&["label", "move", "main", "--view", &base]);
    assert!(refused.contains("label_move_refused"), "{refused}");
    assert!(refused.contains("lacks change"), "{refused}");
    assert!(refused.contains("--force"), "{refused}");
    assert_eq!(local.label_view("main"), frontier);

    let forced = local.ok_json(&["label", "move", "main", "--view", &base, "--force"]);
    assert_eq!(forced["moved"], true, "{forced}");
    assert_eq!(forced["rebound"], false, "{forced}");
    assert_eq!(local.label_view("main"), base);
    // The workspace keeps what it pinned: the label moved away from it.
    let metadata = local.metadata();
    assert_eq!(metadata.basis, view(&frontier));
    assert_eq!(metadata.selected_frontier, None);
    local.ok_json(&["materialize"]);
    assert_eq!(
        std::fs::read_to_string(local.workspace.join("a.txt")).unwrap(),
        "a.txt"
    );

    // Back to the workspace's view is forward again, and rebinds it.
    let back = local.ok_json(&["label", "move", "main"]);
    assert_eq!(back["rebound"], true, "{back}");
    assert_eq!(local.label_view("main"), frontier);
}

#[test]
fn a_move_that_leaves_a_frontier_behind_needs_force() {
    let local = Local::new();
    let base = local.label_view("main");
    let first = local.change("a.txt");
    local.ok_json(&["switch", "main", "--frontier", &base]);
    let second = local.change("b.txt");
    let frontiers = local.ok_json(&["frontier", "list"]);
    assert_eq!(frontiers["frontiers"].as_array().unwrap().len(), 2);

    let refused = local.fails(&["label", "move", "main"]);
    assert!(refused.contains("label_move_refused"), "{refused}");
    assert!(refused.contains("left behind"), "{refused}");
    assert!(refused.contains(&first), "{refused}");
    assert_eq!(local.label_view("main"), base);

    // Elsewhere than the workspace's view, the workspace keeps its selection.
    let elsewhere = local.ok_json(&["label", "move", "main", "--view", &first, "--force"]);
    assert_eq!(elsewhere["rebound"], false, "{elsewhere}");
    let metadata = local.metadata();
    assert_eq!(metadata.basis, view(&base));
    assert_eq!(metadata.selected_frontier, Some(view(&second)));

    let forced = local.ok_json(&["label", "move", "main", "--view", &second, "--force"]);
    assert_eq!(forced["rebound"], true, "{forced}");
    assert_eq!(local.metadata().basis, view(&second));
}

#[test]
fn without_a_workspace_the_view_is_required() {
    let local = Local::new();
    let frontier = local.change("a.txt");
    let store = workspace_store(&local.workspace);
    let store = store.to_str().unwrap();
    let outside = local.workspace.parent().unwrap();

    let refused = fails(
        outside,
        &local.stores,
        &local.config,
        &[
            "--format", "json", "--store", store, "label", "move", "main",
        ],
    );
    assert!(refused.contains("invalid_input"), "{refused}");
    assert!(refused.contains("--view"), "{refused}");

    let moved = ok_json(
        outside,
        &local.stores,
        &local.config,
        &[
            "--store", store, "label", "move", "main", "--view", &frontier,
        ],
    );
    assert_eq!(moved["rebound"], false, "{moved}");
    assert_eq!(local.label_view("main"), frontier);
}

#[test]
fn an_interruption_before_the_head_moves_discards_the_rebind() {
    let local = Local::new();
    let base = local.label_view("main");
    let frontier = local.change("a.txt");

    local.interrupted_move("label-move-staged");
    assert!(local.metadata().pending_repository_operation.is_some());
    assert_eq!(local.label_view("main"), base);

    local.ok_json(&["materialize"]);
    let metadata = local.metadata();
    assert_eq!(metadata.pending_repository_operation, None);
    assert_eq!(metadata.basis, view(&base));
    assert_eq!(metadata.selected_frontier, Some(view(&frontier)));
}

#[test]
fn an_interruption_after_the_head_moves_completes_the_rebind() {
    let local = Local::new();
    let frontier = local.change("a.txt");

    local.interrupted_move("label-move-published");
    assert!(local.metadata().pending_repository_operation.is_some());
    assert_eq!(local.label_view("main"), frontier);

    local.ok_json(&["materialize"]);
    let metadata = local.metadata();
    assert_eq!(metadata.pending_repository_operation, None);
    assert_eq!(metadata.basis, view(&frontier));
    assert_eq!(metadata.observed_label_binding, view(&frontier));
    assert_eq!(metadata.selected_frontier, None);
}
/// Decision 9: a change the label holds can still be revised, selected and squashed.
#[test]
fn a_change_the_label_holds_can_be_revised_selected_and_squashed() {
    let local = Local::new();
    local.change("a.txt");
    let change = local.metadata().selected_change.unwrap().to_string();
    local.ok_json(&["label", "move", "main"]);
    let held = local.label_view("main");

    // Nothing above the label's revision to squash.
    let squashed = local.ok_json(&["change", "squash", &change]);
    assert_eq!(squashed["no_op"], true, "{squashed}");
    local.ok_json(&["change", "select", &change]);

    for value in ["second", "third"] {
        std::fs::write(local.workspace.join("a.txt"), value).unwrap();
        local.ok_json(&["add", "a.txt", "--change", &change, "--message", value]);
    }
    let revised = local.metadata();
    assert_eq!(revised.basis, view(&held));
    let frontier = revised
        .selected_frontier
        .expect("the revision is a frontier");
    assert_eq!(local.label_view("main"), held);

    // The two new revisions collapse; the label's stays.
    let squashed = local.ok_json(&["change", "squash", &change]);
    assert_eq!(squashed["no_op"], false, "{squashed}");
    assert_eq!(
        squashed["rewritten"],
        serde_json::json!([change]),
        "{squashed}"
    );
    assert_ne!(local.metadata().selected_frontier, Some(frontier));

    // The revised view is forward from the label's.
    let moved = local.ok_json(&["label", "move", "main"]);
    assert_eq!(moved["rebound"], true, "{moved}");
    assert_eq!(
        std::fs::read_to_string(local.workspace.join("a.txt")).unwrap(),
        "third"
    );
}
