//! A fresh Wheat workspace driven through the `wheat` binary, for the CLI cases.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};

use super::CaseContext;
use crate::{
    measure::{self, Captured},
    path_text,
};

pub(crate) struct Workspace {
    wheat: PathBuf,
    scratch: PathBuf,
    root: PathBuf,
    stores: PathBuf,
    config: PathBuf,
}

impl Workspace {
    /// Initializes a workspace in the case's scratch directory, with a fixed actor.
    pub(crate) fn new(context: &CaseContext) -> Result<Self> {
        let root = context.scratch.join("workspace");
        let config = context.scratch.join("config");
        fs::create_dir_all(config.join("wheat"))?;
        fs::write(
            config.join("wheat/config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Bench","email":"wheat-bench@example.invalid"}}"#,
        )?;
        let workspace = Self {
            wheat: context.wheat.clone(),
            scratch: context.scratch.clone(),
            stores: context.scratch.join("stores"),
            config,
            root,
        };
        workspace.ok(&["init", path_text(&workspace.root)?])?;
        Ok(workspace)
    }

    /// Runs `wheat --workspace <root> <arguments>`, which must succeed, and measures it.
    pub(crate) fn ok(&self, arguments: &[&str]) -> Result<Captured> {
        let mut command = Command::new(&self.wheat);
        crate::isolate(&mut command)
            .arg("--workspace")
            .arg(&self.root)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config);
        let captured = measure::run(&mut command, &self.scratch)?;
        captured.ensure_success(&format!("wheat {}", arguments.join(" ")))?;
        Ok(captured)
    }

    /// Runs `wheat <arguments>` in `directory`, with this workspace's store home and
    /// config, which must succeed, and measures it. For commands that make or use
    /// another workspace, such as `clone`.
    pub(crate) fn ok_in(&self, directory: &Path, arguments: &[&str]) -> Result<Captured> {
        let mut command = Command::new(&self.wheat);
        crate::isolate(&mut command)
            .current_dir(directory)
            .args(arguments)
            .env("WHEAT_STORE_HOME", &self.stores)
            .env("XDG_CONFIG_HOME", &self.config);
        let captured = measure::run(&mut command, &self.scratch)?;
        captured.ensure_success(&format!("wheat {}", arguments.join(" ")))?;
        Ok(captured)
    }

    /// The directory the workspace was initialized in.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    pub(crate) fn write(&self, path: &str, contents: impl AsRef<[u8]>) -> Result<()> {
        let path = self.path(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, contents).with_context(|| format!("write {}", path.display()))
    }

    pub(crate) fn read(&self, path: &str) -> Result<String> {
        let path = self.path(path);
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    }
}

/// The value of the `<name> <value>` line in a command's human output.
pub(crate) fn field(captured: &Captured, name: &str) -> Result<String> {
    stdout(captured)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")).map(str::to_owned))
        .with_context(|| format!("no {name:?} line in {}", stdout(captured)))
}

pub(crate) fn stdout(captured: &Captured) -> String {
    String::from_utf8_lossy(&captured.stdout).into_owned()
}
