//! Two clones of one served repository, for tests of what passes between them.

use std::path::{Path, PathBuf};

use wheat_client::workspace_metadata::WorkspaceMetadata;

use wheat_server::server::ServerConfig;

use super::{fails, fails_json, ok, ok_json, serve_configured, write_config};

pub struct Side {
    pub workspace: PathBuf,
    pub stores: PathBuf,
}

impl Side {
    pub fn ok_json(&self, config: &Path, args: &[&str]) -> serde_json::Value {
        ok_json(&self.workspace, &self.stores, config, args)
    }

    pub fn fails(&self, config: &Path, args: &[&str]) -> String {
        fails(&self.workspace, &self.stores, config, args)
    }

    pub fn fails_json(&self, config: &Path, args: &[&str]) -> serde_json::Value {
        fails_json(&self.workspace, &self.stores, config, args)
    }

    /// A new change writing `path`; the workspace selects it as a frontier.
    pub fn change(&self, config: &Path, path: &str) -> String {
        self.ok_json(
            config,
            &[
                "change",
                "new",
                "--path",
                path,
                "--value",
                path,
                "--message",
                path,
            ],
        );
        WorkspaceMetadata::read(&self.workspace)
            .unwrap()
            .selected_frontier
            .unwrap()
            .to_string()
    }

    pub fn label_view(&self, config: &Path, name: &str) -> Option<String> {
        self.ok_json(config, &["labels"])["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|label| label["name"] == name)
            .map(|label| label["view"].as_str().unwrap().to_owned())
    }
}

pub struct Sides {
    pub root: tempfile::TempDir,
    pub config: PathBuf,
    pub writer: Side,
    pub reader: Side,
}

/// Two clones, `writer` and `reader`, of an empty repository the returned server
/// serves.
pub fn sides() -> (Sides, super::Server) {
    sides_configured(ServerConfig::new())
}

/// Like [`sides`], with the server configured by `server`.
pub fn sides_configured(server: ServerConfig) -> (Sides, super::Server) {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    write_config(&config);
    let server = serve_configured(root.path(), &config, server);
    let clone = |name: &str| {
        let side = Side {
            workspace: root.path().join(name),
            stores: root.path().join(format!("{name}-stores")),
        };
        ok(
            root.path(),
            &side.stores,
            &config,
            &["clone", &server.url, side.workspace.to_str().unwrap()],
        );
        side
    };
    let writer = clone("writer");
    let reader = clone("reader");
    (
        Sides {
            root,
            config,
            writer,
            reader,
        },
        server,
    )
}
