use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    ChangeId, MaterializedRootsRecord, PendingRepositoryCasRecord, RepositoryEnvelopeId,
    RepositoryRootRecord, StoreMetadataRecord, TreeId, ViewId, WorkspaceMetadataRecord,
    decode_store_metadata, decode_workspace_metadata, encode_store_metadata,
    encode_workspace_metadata,
};
use wheat_store::RepositoryHead;

/// A store's identity, in its root.
pub const STORE_METADATA_FILE: &str = "store.meta";
/// A workspace's metadata, in its `.wheat` directory.
pub const WORKSPACE_METADATA_FILE: &str = "workspace.meta";
/// The most bytes a store or workspace metadata file may hold.
const MAX_METADATA_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreMetadata {
    pub store_id: Uuid,
}

/// A workspace's binding to its store and label, kept in `.wheat/workspace.meta`.
/// Serde serves only the rewrite transaction journal, which holds a workspace's old
/// and new metadata while a rewrite is in flight.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMetadata {
    pub workspace_id: Uuid,
    pub registration_token: Uuid,
    pub store: WorkspaceStore,
    pub active_label: String,
    pub basis: ViewId,
    pub observed_label_binding: ViewId,
    pub selected_frontier: Option<ViewId>,
    pub materialized: Option<MaterializedState>,
    pub selected_change: Option<ChangeId>,
    pub pending_repository_operation: Option<PendingRepositoryOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRepositoryOperation {
    pub expected_head: SerializedHead,
    pub proposed_head: SerializedHead,
    pub resulting_frontier: ViewId,
    pub selected_change: Option<ChangeId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStore {
    pub path: PathBuf,
    pub store_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedState {
    pub repository_head: SerializedHead,
    pub view: ViewId,
    pub tree: TreeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerializedHead {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
}

pub struct PreparedWorkspaceMetadata {
    temporary: tempfile::NamedTempFile,
    destination: PathBuf,
}

#[derive(Debug, Error)]
pub enum WorkspaceMetadataPublishError {
    #[error("workspace metadata was not published")]
    BeforePublish(#[source] io::Error),
    #[error("workspace metadata was published but its directory sync failed")]
    PublishedButUnsynced(#[source] io::Error),
}

impl From<RepositoryHead> for SerializedHead {
    fn from(head: RepositoryHead) -> Self {
        Self {
            generation: head.generation,
            envelope: head.envelope,
        }
    }
}

impl From<SerializedHead> for RepositoryHead {
    fn from(head: SerializedHead) -> Self {
        Self {
            generation: head.generation,
            envelope: head.envelope,
        }
    }
}

impl From<SerializedHead> for RepositoryRootRecord {
    fn from(head: SerializedHead) -> Self {
        Self {
            generation: head.generation,
            envelope: head.envelope,
        }
    }
}

impl From<RepositoryRootRecord> for SerializedHead {
    fn from(head: RepositoryRootRecord) -> Self {
        Self {
            generation: head.generation,
            envelope: head.envelope,
        }
    }
}

impl From<&MaterializedState> for MaterializedRootsRecord {
    fn from(state: &MaterializedState) -> Self {
        Self {
            repository_head: state.repository_head.into(),
            view: state.view,
            tree: state.tree,
        }
    }
}

impl From<MaterializedRootsRecord> for MaterializedState {
    fn from(record: MaterializedRootsRecord) -> Self {
        Self {
            repository_head: record.repository_head.into(),
            view: record.view,
            tree: record.tree,
        }
    }
}

impl StoreMetadata {
    pub fn create(root: &Path) -> Result<Self> {
        let metadata = Self {
            store_id: Uuid::now_v7(),
        };
        let bytes = encode_store_metadata(StoreMetadataRecord {
            store_id: metadata.store_id,
        })?;
        write_new(&root.join(STORE_METADATA_FILE), &bytes)?;
        Ok(metadata)
    }

    pub fn read(root: &Path) -> Result<Self> {
        let path = root.join(STORE_METADATA_FILE);
        let record = decode_store_metadata(&read_bounded(&path)?)
            .with_context(|| format!("unsupported Wheat store metadata at {}", path.display()))?;
        Ok(Self {
            store_id: record.store_id,
        })
    }
}

impl WorkspaceMetadata {
    /// A workspace on `active_label` at `basis`, pinning `selected_frontier` when the
    /// label had one frontier to select. Materializing reads only what is pinned here.
    pub fn new(
        store_path: PathBuf,
        store_id: Uuid,
        active_label: String,
        basis: ViewId,
        selected_frontier: Option<ViewId>,
    ) -> Self {
        Self {
            workspace_id: Uuid::now_v7(),
            registration_token: Uuid::now_v7(),
            store: WorkspaceStore {
                path: store_path,
                store_id,
            },
            active_label,
            basis,
            observed_label_binding: basis,
            selected_frontier,
            materialized: None,
            selected_change: None,
            pending_repository_operation: None,
        }
    }

    pub fn effective_view(&self) -> ViewId {
        self.selected_frontier.unwrap_or(self.basis)
    }

    /// The metadata file of the workspace at `root`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".wheat").join(WORKSPACE_METADATA_FILE)
    }

    pub fn read(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        Self::decode(&read_bounded(&path)?)
            .with_context(|| format!("unsupported Wheat workspace metadata at {}", path.display()))
    }

    /// Decodes a workspace metadata file's bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let record = decode_workspace_metadata(bytes)?;
        Ok(Self {
            workspace_id: record.workspace_id,
            registration_token: record.registration_token,
            store: WorkspaceStore {
                path: PathBuf::from(record.store_path),
                store_id: record.store_id,
            },
            active_label: record.active_label,
            basis: record.basis,
            observed_label_binding: record.observed_label_binding,
            selected_frontier: record.selected_frontier,
            materialized: record.materialized.map(MaterializedState::from),
            selected_change: record.selected_change,
            pending_repository_operation: record.pending_repository_operation.map(
                |(cas, selected_change)| PendingRepositoryOperation {
                    expected_head: cas.expected_head.into(),
                    proposed_head: cas.proposed_head.into(),
                    resulting_frontier: cas.resulting_frontier,
                    selected_change,
                },
            ),
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let Some(store_path) = self.store.path.to_str() else {
            bail!(
                "workspace store path {} is not UTF-8",
                self.store.path.display()
            );
        };
        Ok(encode_workspace_metadata(&WorkspaceMetadataRecord {
            workspace_id: self.workspace_id,
            registration_token: self.registration_token,
            store_path: store_path.to_owned(),
            store_id: self.store.store_id,
            active_label: self.active_label.clone(),
            basis: self.basis,
            observed_label_binding: self.observed_label_binding,
            selected_frontier: self.selected_frontier,
            materialized: self
                .materialized
                .as_ref()
                .map(MaterializedRootsRecord::from),
            selected_change: self.selected_change,
            pending_repository_operation: self.pending_repository_operation.as_ref().map(
                |operation| {
                    (
                        PendingRepositoryCasRecord {
                            expected_head: operation.expected_head.into(),
                            proposed_head: operation.proposed_head.into(),
                            resulting_frontier: operation.resulting_frontier,
                        },
                        operation.selected_change,
                    )
                },
            ),
        })?)
    }

    pub fn initialize(&self, root: &Path) -> Result<()> {
        let control = root.join(".wheat");
        let bytes = self.encode()?;
        fs::create_dir(&control)
            .with_context(|| format!("create workspace control directory {}", control.display()))?;
        write_new(&Self::path(root), &bytes)
    }

    pub fn replace_prepared(&self, root: &Path) -> Result<PreparedWorkspaceMetadata> {
        let bytes = self.encode()?;
        let destination = Self::path(root);
        let parent = destination
            .parent()
            .context("workspace metadata has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        Ok(PreparedWorkspaceMetadata {
            temporary,
            destination,
        })
    }
}

impl PreparedWorkspaceMetadata {
    pub fn publish(self) -> Result<(), WorkspaceMetadataPublishError> {
        self.temporary
            .persist(&self.destination)
            .map_err(|error| WorkspaceMetadataPublishError::BeforePublish(error.error))?;
        sync_parent(&self.destination).map_err(WorkspaceMetadataPublishError::PublishedButUnsynced)
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        bail!("{} exceeds {MAX_METADATA_BYTES} bytes", path.display());
    }
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_parent(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_metadata_round_trips_and_refuses_other_bytes() {
        let root = tempfile::tempdir().unwrap();
        let created = StoreMetadata::create(root.path()).unwrap();
        assert_eq!(StoreMetadata::read(root.path()).unwrap(), created);
        let path = root.path().join(STORE_METADATA_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0);
        std::fs::write(&path, &bytes).unwrap();
        assert!(StoreMetadata::read(root.path()).is_err());
        std::fs::write(&path, vec![0; MAX_METADATA_BYTES as usize + 1]).unwrap();
        let error = StoreMetadata::read(root.path()).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds"), "{error:#}");
    }

    #[test]
    fn prepared_workspace_metadata_publishes_atomically() {
        let root = tempfile::tempdir().unwrap();
        let store_id = Uuid::now_v7();
        let basis = ViewId::from_digest([3; 32]);
        let original = WorkspaceMetadata::new(
            PathBuf::from("/store"),
            store_id,
            "main".into(),
            basis,
            None,
        );
        original.initialize(root.path()).unwrap();
        let mut replacement = original.clone();
        replacement.selected_frontier = Some(ViewId::from_digest([4; 32]));
        replacement
            .replace_prepared(root.path())
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(WorkspaceMetadata::read(root.path()).unwrap(), replacement);
    }

    #[test]
    fn workspace_metadata_round_trips_every_root_and_effective_view() {
        let root = tempfile::tempdir().unwrap();
        let basis = ViewId::from_digest([7; 32]);
        let frontier = ViewId::from_digest([8; 32]);
        let head = SerializedHead {
            generation: 9,
            envelope: RepositoryEnvelopeId::from_digest([9; 32]),
        };
        let mut metadata = WorkspaceMetadata::new(
            PathBuf::from("/store"),
            Uuid::now_v7(),
            "main".into(),
            basis,
            Some(frontier),
        );
        metadata.selected_change = Some(wheat_core::ChangeId::new());
        metadata.materialized = Some(MaterializedState {
            repository_head: head,
            view: frontier,
            tree: TreeId::from_digest([10; 32]),
        });
        metadata.pending_repository_operation = Some(PendingRepositoryOperation {
            expected_head: head,
            proposed_head: SerializedHead {
                generation: 10,
                envelope: RepositoryEnvelopeId::from_digest([11; 32]),
            },
            resulting_frontier: basis,
            selected_change: None,
        });
        metadata.initialize(root.path()).unwrap();
        assert_eq!(WorkspaceMetadata::read(root.path()).unwrap(), metadata);
        assert_eq!(metadata.effective_view(), frontier);
        let bytes = std::fs::read(WorkspaceMetadata::path(root.path())).unwrap();
        assert_eq!(WorkspaceMetadata::decode(&bytes).unwrap(), metadata);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_metadata_refuses_a_store_path_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;

        let root = tempfile::tempdir().unwrap();
        let metadata = WorkspaceMetadata::new(
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/store-\xff")),
            Uuid::now_v7(),
            "main".into(),
            ViewId::from_digest([1; 32]),
            None,
        );
        let error = metadata.initialize(root.path()).unwrap_err();
        assert!(format!("{error:#}").contains("not UTF-8"), "{error:#}");
        assert!(!root.path().join(".wheat").exists());
    }
}
