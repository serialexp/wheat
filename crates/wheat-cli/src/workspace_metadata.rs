use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use wheat_core::{ChangeId, RepositoryEnvelopeId, TreeId, ViewId};
use wheat_store::RepositoryHead;

const STORE_FORMAT: &str = "wheat-store";
const WORKSPACE_FORMAT: &str = "wheat-workspace";
const STORE_FORMAT_VERSION: u32 = 1;
const LEGACY_WORKSPACE_FORMAT_VERSION: u32 = 3;
const WORKSPACE_FORMAT_VERSION: u32 = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoreMetadata {
    format: String,
    version: u32,
    pub store_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceMetadata {
    format: String,
    version: u32,
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
pub(crate) struct PendingRepositoryOperation {
    pub expected_head: SerializedHead,
    pub proposed_head: SerializedHead,
    pub resulting_frontier: ViewId,
    pub selected_change: Option<ChangeId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceStore {
    pub path: PathBuf,
    pub store_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedState {
    pub repository_head: SerializedHead,
    pub view: ViewId,
    pub tree: TreeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SerializedHead {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
}

pub(crate) struct PreparedWorkspaceMetadata {
    temporary: tempfile::NamedTempFile,
    destination: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum WorkspaceMetadataPublishError {
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

impl StoreMetadata {
    pub(crate) fn create(root: &Path) -> Result<Self> {
        let metadata = Self {
            format: STORE_FORMAT.into(),
            version: STORE_FORMAT_VERSION,
            store_id: Uuid::now_v7(),
        };
        write_json_new(&root.join("store.json"), &metadata)?;
        Ok(metadata)
    }

    pub(crate) fn read(root: &Path) -> Result<Self> {
        let path = root.join("store.json");
        let metadata: Self = read_json(&path)?;
        if metadata.format != STORE_FORMAT || metadata.version != STORE_FORMAT_VERSION {
            bail!("unsupported Wheat store metadata at {}", path.display());
        }
        Ok(metadata)
    }
}

impl WorkspaceMetadata {
    pub(crate) fn new(
        store_path: PathBuf,
        store_id: Uuid,
        active_label: String,
        basis: ViewId,
    ) -> Self {
        Self {
            format: WORKSPACE_FORMAT.into(),
            version: WORKSPACE_FORMAT_VERSION,
            workspace_id: Uuid::now_v7(),
            registration_token: Uuid::now_v7(),
            store: WorkspaceStore {
                path: store_path,
                store_id,
            },
            active_label,
            basis,
            observed_label_binding: basis,
            selected_frontier: None,
            materialized: None,
            selected_change: None,
            pending_repository_operation: None,
        }
    }

    pub(crate) fn effective_view(&self) -> ViewId {
        self.selected_frontier.unwrap_or(self.basis)
    }

    pub(crate) fn read(root: &Path) -> Result<Self> {
        let path = root.join(".wheat/workspace.json");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .context("workspace metadata lacks a numeric version")?;
        if version == u64::from(LEGACY_WORKSPACE_FORMAT_VERSION) {
            let object = value
                .as_object_mut()
                .context("workspace metadata must be a JSON object")?;
            object.insert(
                "version".into(),
                serde_json::Value::from(WORKSPACE_FORMAT_VERSION),
            );
            let workspace_id = object
                .get("workspace_id")
                .and_then(serde_json::Value::as_str)
                .context("legacy workspace metadata lacks a workspace identity")?;
            object.insert(
                "registration_token".into(),
                serde_json::Value::String(workspace_id.to_owned()),
            );
        } else if version != u64::from(WORKSPACE_FORMAT_VERSION) {
            bail!("unsupported Wheat workspace metadata at {}", path.display());
        }
        let metadata: Self =
            serde_json::from_value(value).with_context(|| format!("decode {}", path.display()))?;
        if metadata.format != WORKSPACE_FORMAT
            || metadata.version != WORKSPACE_FORMAT_VERSION
            || metadata.workspace_id.is_nil()
            || metadata.registration_token.is_nil()
        {
            bail!("unsupported Wheat workspace metadata at {}", path.display());
        }
        Ok(metadata)
    }

    pub(crate) fn initialize(&self, root: &Path) -> Result<()> {
        let control = root.join(".wheat");
        fs::create_dir(&control)
            .with_context(|| format!("create workspace control directory {}", control.display()))?;
        write_json_new(&control.join("workspace.json"), self)
    }

    pub(crate) fn replace_prepared(&self, root: &Path) -> Result<PreparedWorkspaceMetadata> {
        let destination = root.join(".wheat/workspace.json");
        let parent = destination
            .parent()
            .context("workspace metadata has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        Ok(PreparedWorkspaceMetadata {
            temporary,
            destination,
        })
    }
}

impl PreparedWorkspaceMetadata {
    pub(crate) fn publish(self) -> Result<(), WorkspaceMetadataPublishError> {
        self.temporary
            .persist(&self.destination)
            .map_err(|error| WorkspaceMetadataPublishError::BeforePublish(error.error))?;
        sync_parent(&self.destination).map_err(WorkspaceMetadataPublishError::PublishedButUnsynced)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
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
    fn store_metadata_round_trips_and_rejects_unknown_fields() {
        let root = tempfile::tempdir().unwrap();
        let created = StoreMetadata::create(root.path()).unwrap();
        assert_eq!(StoreMetadata::read(root.path()).unwrap(), created);
        std::fs::write(
            root.path().join("store.json"),
            format!(
                "{{\"format\":\"wheat-store\",\"version\":1,\"store_id\":\"{}\",\"extra\":true}}",
                created.store_id
            ),
        )
        .unwrap();
        assert!(StoreMetadata::read(root.path()).is_err());
    }

    #[test]
    fn prepared_workspace_metadata_publishes_atomically() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".wheat")).unwrap();
        let store_id = Uuid::now_v7();
        let basis = ViewId::from_digest([3; 32]);
        let original =
            WorkspaceMetadata::new(PathBuf::from("/store"), store_id, "main".into(), basis);
        write_json_new(&root.path().join(".wheat/workspace.json"), &original).unwrap();
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
    fn workspace_v4_round_trips_pinned_label_and_effective_view() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".wheat")).unwrap();
        let basis = ViewId::from_digest([7; 32]);
        let frontier = ViewId::from_digest([8; 32]);
        let mut metadata = WorkspaceMetadata::new(
            PathBuf::from("/store"),
            Uuid::now_v7(),
            "main".into(),
            basis,
        );
        metadata.selected_frontier = Some(frontier);
        metadata.selected_change = Some(wheat_core::ChangeId::new());
        write_json_new(&root.path().join(".wheat/workspace.json"), &metadata).unwrap();
        assert_eq!(WorkspaceMetadata::read(root.path()).unwrap(), metadata);
        assert_eq!(metadata.effective_view(), frontier);
        let document: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.path().join(".wheat/workspace.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document["version"], 4);
        assert_eq!(
            document["registration_token"],
            metadata.registration_token.to_string()
        );
        assert_eq!(document["active_label"], "main");
        assert!(document.get("selection").is_none());
    }
}
