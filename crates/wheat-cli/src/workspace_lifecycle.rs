use std::{
    ffi::CString,
    fmt::Write as _,
    fs,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::{
    command_output::Success,
    discovery::{canonical_store_path, discover_workspace},
    materialization::WorkspaceLock,
    workspace_metadata::{StoreMetadata, WorkspaceMetadata},
    workspace_registry::WorkspaceRegistry,
};

const OWNED_CONTROL_ENTRIES: &[&str] = &[
    "workspace.json",
    "workspace.lock",
    "materialization.json",
    "materialization-transactions",
];

pub(crate) fn list(workspace: &Path, explicit_store: Option<&Path>) -> Result<Success> {
    let current = discover_workspace(workspace).ok();
    let store_root = if let Some(store) = explicit_store {
        canonical_store_path(store)?
    } else {
        current
            .as_ref()
            .map(|context| context.store_root.clone())
            .context("workspace list requires a discoverable workspace or --store")?
    };
    let store = StoreMetadata::read(&store_root)?;
    let local = crate::local::open(&store_root)?;
    let _operation = crate::rewrite_transaction::StoreOperationGuard::acquire(&store_root, &local)?;
    let registry = WorkspaceRegistry::new(&store_root, store.store_id)?;
    let pins = registry.enumerate()?;
    let current_id = current.as_ref().and_then(|context| {
        (context.store_root == store_root).then_some(context.workspace.workspace_id)
    });

    let mut rows = Vec::with_capacity(pins.len());
    let mut human = String::new();
    for pin in pins {
        let status = workspace_status(&pin.workspace_path, &pin);
        let is_current = current_id == Some(pin.workspace_id);
        writeln!(
            human,
            "{}{}\t{}\t{status}",
            if is_current { "* " } else { "  " },
            pin.workspace_id,
            pin.workspace_path.display()
        )?;
        rows.push(json!({
            "workspace_id": pin.workspace_id.to_string(),
            "path": pin.workspace_path,
            "current": is_current,
            "status": status,
            "effective_view": pin.stable.selected_frontier.unwrap_or(pin.stable.basis).to_string(),
        }));
    }
    Success::new("workspace.list", json!({ "workspaces": rows }), human)
}

fn workspace_status(path: &Path, pin: &crate::workspace_registry::WorkspacePin) -> &'static str {
    if !path.exists() {
        return "missing";
    }
    let control = path.join(".wheat");
    match fs::symlink_metadata(&control) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return "identity_mismatch";
        }
        Ok(_) => {}
        Err(_) => return "inaccessible",
    }
    match WorkspaceMetadata::read(path) {
        Ok(metadata)
            if metadata.workspace_id == pin.workspace_id
                && metadata.registration_token == pin.registration_token
                && metadata.store.store_id == pin.store_id =>
        {
            "attached"
        }
        Ok(_) => "identity_mismatch",
        Err(_) => "inaccessible",
    }
}

pub(crate) fn unregister(workspace: &Path, explicit_store: Option<&Path>) -> Result<Success> {
    let context = discover_workspace(workspace)?;
    if let Some(store) = explicit_store {
        ensure!(
            canonical_store_path(store)? == context.store_root,
            "explicit Wheat store does not match the discovered workspace link"
        );
    }
    let local = crate::local::open(&context.store_root)?;
    let _operation =
        crate::rewrite_transaction::StoreOperationGuard::acquire(&context.store_root, &local)?;
    let lock = WorkspaceLock::acquire(&context.root)?;
    let mut metadata = WorkspaceMetadata::read(&context.root)?;
    crate::runtime()?.block_on(crate::materialization::recover_locked(
        &context.root,
        &local,
        &mut metadata,
    ))?;
    crate::selection_recovery::recover(&context.root, &local, &mut metadata)?;
    ensure!(
        metadata.workspace_id == context.workspace.workspace_id
            && metadata.registration_token == context.workspace.registration_token
            && metadata.store.store_id == context.workspace.store.store_id,
        "workspace identity changed before unregister"
    );
    let registry = WorkspaceRegistry::new(&context.store_root, metadata.store.store_id)?;
    let pin = registry.read(metadata.workspace_id)?;
    ensure!(
        pin.registration_token == metadata.registration_token,
        "workspace registration token does not match the store registry"
    );
    ensure!(
        pin.pending_materialization.is_none() && pin.pending_repository_cas.is_none(),
        "workspace has an unresolved operation; run a workspace command to recover it first"
    );
    ensure_control_directory_owned(lock.control())?;

    // Remove the authenticated local attachment first. A crash before pin removal leaks a
    // conservative stale pin, which is safe; the inverse ordering could leave a live workspace
    // unpinned and let a future collector reclaim its selected history.
    remove_control_directory(&context.root, lock.control())?;
    registry.remove(metadata.workspace_id, metadata.registration_token)?;

    Success::new(
        "workspace.unregister",
        json!({
            "workspace_id": metadata.workspace_id.to_string(),
            "path": context.root,
        }),
        format!(
            "detached Wheat workspace {}; materialized files were preserved\n",
            context.root.display()
        ),
    )
}

fn ensure_control_directory_owned(control: &fs::File) -> Result<()> {
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", control.as_raw_fd()));
    for entry in fs::read_dir(&proc_path)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|_| {
            anyhow::anyhow!("workspace control directory contains a non-UTF-8 name")
        })?;
        if !OWNED_CONTROL_ENTRIES.contains(&name.as_str()) {
            bail!("workspace control directory contains unknown entry {name}");
        }
        let entry_metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !entry_metadata.file_type().is_symlink(),
            "workspace control entry {name} is a symlink"
        );
        if name == "materialization-transactions" {
            ensure!(
                entry_metadata.is_dir(),
                "materialization transaction path is not a directory"
            );
            ensure!(
                fs::read_dir(entry.path())?.next().is_none(),
                "workspace has materialization transaction debris; recover it before unregistering"
            );
        } else {
            ensure!(
                entry_metadata.is_file(),
                "workspace control entry {name} is not a file"
            );
        }
    }
    Ok(())
}

fn remove_control_directory(root: &Path, control: &fs::File) -> Result<()> {
    for name in [
        "materialization-transactions",
        "materialization.json",
        "workspace.json",
        "workspace.lock",
    ] {
        unlink_control_entry(control, name, name == "materialization-transactions")?;
    }
    // The name must still resolve to the directory held by the lock; otherwise a concurrent rename
    // occurred and removing by path could detach an unrelated replacement.
    let current = fs::File::open(root.join(".wheat"))?;
    ensure!(
        current.metadata()?.ino() == control.metadata()?.ino()
            && current.metadata()?.dev() == control.metadata()?.dev(),
        "workspace control directory changed during unregister"
    );
    fs::remove_dir(root.join(".wheat"))?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn unlink_control_entry(control: &fs::File, name: &str, directory: bool) -> Result<()> {
    let name = CString::new(name)?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    let result = unsafe { libc::unlinkat(control.as_raw_fd(), name.as_ptr(), flags) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error.into())
    }
}
