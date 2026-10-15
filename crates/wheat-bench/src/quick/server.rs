//! A wheat-server running in the case's process, for the protocol cases.

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::{runtime::Runtime, sync::oneshot, task::JoinHandle};
use wheat_server::{
    registry::Registry,
    server::{ServerConfig, run_until},
};

use super::cpus::CpuSet;

/// Serves one workspace's store as `bench/repo` until [`Server::stop`].
pub(crate) struct Server {
    runtime: Runtime,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
    store: PathBuf,
    url: String,
}

impl Server {
    /// Serves the store of the workspace at `workspace`.
    pub(crate) fn start(workspace: &Path) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        Self::start_in(runtime, workspace)
    }

    /// Like [`Self::start`], but every server thread runs on `cpus`, with one worker
    /// thread per CPU.
    pub(crate) fn start_pinned(workspace: &Path, cpus: &CpuSet) -> Result<Self> {
        let pinned = cpus.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cpus.len())
            .on_thread_start(move || {
                pinned
                    .pin_current_thread()
                    .expect("pin a server thread to its CPUs");
            })
            .enable_all()
            .build()?;
        Self::start_in(runtime, workspace)
    }

    fn start_in(runtime: Runtime, workspace: &Path) -> Result<Self> {
        let store = workspace_store(workspace)?;
        let registry = Registry::single_named("bench/repo".to_owned(), &store)?;
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = runtime.spawn(run_until(
            "127.0.0.1:0".parse()?,
            registry,
            ServerConfig::new(),
            ready_tx,
            shutdown_rx,
        ));
        let addr: SocketAddr = runtime.block_on(ready_rx).context("server did not start")?;
        Ok(Self {
            runtime,
            shutdown,
            task,
            store,
            url: format!("wheat://{addr}/bench/repo"),
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Stops serving and returns the store, which the case may then open itself.
    pub(crate) fn stop(self) -> Result<PathBuf> {
        let _ = self.shutdown.send(());
        self.runtime.block_on(self.task)??;
        Ok(self.store)
    }
}

fn workspace_store(workspace: &Path) -> Result<PathBuf> {
    let document: Value =
        serde_json::from_slice(&fs::read(workspace.join(".wheat/workspace.json"))?)?;
    let path = document["store"]["path"]
        .as_str()
        .context("workspace.json has no store path")?;
    Ok(PathBuf::from(path))
}
