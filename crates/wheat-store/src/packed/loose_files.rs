//! Keeping the catalog's record of loose files (see `catalog::loose_index`) in step
//! with the loose directories: settling what a crash left pending, and listing the
//! directories once for a new index.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use super::catalog::{CatalogError, LooseKind, PackedCatalog};

/// The loose directories of a packed store.
#[derive(Clone, Debug)]
pub(super) struct LooseDirectories {
    pub(super) objects: PathBuf,
    pub(super) content: PathBuf,
}

impl LooseDirectories {
    pub(super) fn of(root: &Path) -> Self {
        Self {
            objects: root.join("loose-objects"),
            content: root.join("loose-content"),
        }
    }

    fn directory(&self, kind: LooseKind) -> &Path {
        match kind {
            LooseKind::Object => &self.objects,
            LooseKind::Content => &self.content,
        }
    }

    /// Where the loose file of `id` is: `{first two hex digits}/{the rest}`, as
    /// `LooseObjectStore::path` and `ContentStore::path` name it.
    pub(super) fn path(&self, kind: LooseKind, id: &[u8; 32]) -> PathBuf {
        let hex = hex::encode(id);
        self.directory(kind).join(&hex[..2]).join(&hex[2..])
    }

    /// The length of `id`'s loose file, or `None` if there is none.
    pub(super) fn length(&self, kind: LooseKind, id: &[u8; 32]) -> io::Result<Option<u64>> {
        match fs::metadata(self.path(kind, id)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// What is on disk for each of `ids`, to settle them with.
    pub(super) fn lengths(
        &self,
        kind: LooseKind,
        ids: &[[u8; 32]],
    ) -> io::Result<Vec<([u8; 32], Option<u64>)>> {
        ids.iter()
            .map(|id| Ok((*id, self.length(kind, id)?)))
            .collect()
    }

    /// Records the loose files in `catalog` as they are on disk: by one listing for a
    /// new index, else by settling only what a crash left pending. Temporary files of
    /// crashed writes go too. The caller holds the store's writer lock.
    pub(super) fn reconcile(&self, catalog: &mut PackedCatalog) -> Result<(), CatalogError> {
        if !catalog.loose_recorded()? {
            let mut files = Vec::new();
            for kind in [LooseKind::Object, LooseKind::Content] {
                list(self.directory(kind), kind, &mut files)?;
            }
            return catalog.record_loose(&files);
        }
        let pending = catalog.pending_loose()?;
        for kind in [LooseKind::Object, LooseKind::Content] {
            let mut settled = Vec::new();
            for (_, id) in pending.iter().filter(|(pending, _)| *pending == kind) {
                let path = self.path(kind, id);
                remove_temporaries_of(&path, id)?;
                settled.push((*id, self.length(kind, id)?));
            }
            catalog.settle_loose(kind, &settled)?;
        }
        Ok(())
    }
}

/// Whether `name` is a temporary file of a loose write: `.{id}.` and `.tmp`
/// around the writer's process and sequence numbers.
fn is_temporary(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp")
}

/// Appends every loose file under `directory` to `files`, removing temporary files.
/// Names that are neither are left alone.
fn list(
    directory: &Path,
    kind: LooseKind,
    files: &mut Vec<(LooseKind, [u8; 32], u64)>,
) -> Result<(), CatalogError> {
    let shards = match fs::read_dir(directory) {
        Ok(shards) => shards,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for shard in shards {
        let shard = shard?;
        let Some(prefix) = shard.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if prefix.len() != 2 || !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !entry.file_type()?.is_file() {
                continue;
            }
            if is_temporary(&name) {
                fs::remove_file(entry.path())?;
                continue;
            }
            let mut id = [0_u8; 32];
            if hex::decode_to_slice(format!("{prefix}{name}"), &mut id).is_ok()
                && hex::encode(id) == format!("{prefix}{name}")
            {
                files.push((kind, id, entry.metadata()?.len()));
            }
        }
    }
    Ok(())
}

/// Removes the temporary files a crashed write of `id` left beside `path`.
fn remove_temporaries_of(path: &Path, id: &[u8; 32]) -> io::Result<()> {
    let Some(shard) = path.parent() else {
        return Ok(());
    };
    let prefix = format!(".{}.", hex::encode(id));
    let entries = match fs::read_dir(shard) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix) && is_temporary(name))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
