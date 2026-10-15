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
        let workspace = Self::configure(context)?;
        workspace.ok(&["init", path_text(&workspace.root)?])?;
        Ok(workspace)
    }

    /// Imports the Git repository `source` into a workspace in the case's scratch
    /// directory, with a fixed actor, and measures the import.
    pub(crate) fn import(context: &CaseContext, source: &Path) -> Result<(Self, Captured)> {
        let workspace = Self::configure(context)?;
        let captured = workspace.ok_in(
            &context.scratch,
            &["import", path_text(source)?, path_text(&workspace.root)?],
        )?;
        Ok((workspace, captured))
    }

    fn configure(context: &CaseContext) -> Result<Self> {
        let config = context.scratch.join("config");
        fs::create_dir_all(config.join("wheat"))?;
        fs::write(
            config.join("wheat/config.json"),
            r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Bench","email":"wheat-bench@example.invalid"}}"#,
        )?;
        Ok(Self {
            wheat: context.wheat.clone(),
            scratch: context.scratch.clone(),
            stores: context.scratch.join("stores"),
            config,
            root: context.scratch.join("workspace"),
        })
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
        self.ok_in_stores(directory, &self.stores, arguments)
    }

    /// Like [`Self::ok_in`], but with the store home `stores`. Workspaces cloned from
    /// one remote into one store home share a store, so separate clients of a server
    /// need separate store homes.
    pub(crate) fn ok_in_stores(
        &self,
        directory: &Path,
        stores: &Path,
        arguments: &[&str],
    ) -> Result<Captured> {
        let captured = self.run_in_stores(directory, stores, arguments)?;
        captured.ensure_success(&format!("wheat {}", arguments.join(" ")))?;
        Ok(captured)
    }

    /// Like [`Self::ok_in_stores`], but the command may fail; the caller looks at how.
    pub(crate) fn run_in_stores(
        &self,
        directory: &Path,
        stores: &Path,
        arguments: &[&str],
    ) -> Result<Captured> {
        let mut command = Command::new(&self.wheat);
        crate::isolate(&mut command)
            .current_dir(directory)
            .args(arguments)
            .env("WHEAT_STORE_HOME", stores)
            .env("XDG_CONFIG_HOME", &self.config);
        measure::run(&mut command, &self.scratch)
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
