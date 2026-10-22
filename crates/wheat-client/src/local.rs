//! Canonical local repository authority; YAML snapshots are never read or written.
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use wheat_core::{Repository, RevisionId, RevisionRecord, Tree};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, PackedObjectStore,
    RepositoryPublisher, RepositoryReader,
};

/// Every local store keeps its objects and content in one packed store under `storage/`.
pub struct LocalRepository {
    pub store_id: uuid::Uuid,
    pub objects: PackedObjectStore,
    pub content: ContentStore,
    pub heads: LocalPublicationStore,
}

pub fn content_store(root: &Path) -> Result<ContentStore> {
    Ok(PackedObjectStore::open(storage_path(root)?)?.content_store())
}

/// Validates the store layout and returns its packed storage directory.
fn storage_path(root: &Path) -> Result<PathBuf> {
    if root.join(".wheat/repository.head").exists() || root.join(".wheat/repository.yaml").exists()
    {
        bail!(
            "embedded prototype Wheat repository detected at {}; discard it and re-import or re-initialize into a separate store",
            root.display()
        );
    }
    if !root.join("store.json").is_file() {
        bail!("cannot open Wheat store: {}", root.display());
    }
    let storage = root.join("storage");
    if !root.join("repository.head").is_file() || !storage.is_dir() {
        bail!("invalid Wheat store layout at {}", root.display());
    }
    Ok(storage)
}

pub fn projection_path(root: &Path) -> Result<PathBuf> {
    storage_path(root)?;
    Ok(root.join("projections/git.git"))
}

#[tracing::instrument(name = "open_store", level = "debug", skip_all)]
pub fn open(root: &Path) -> Result<LocalRepository> {
    let store_id = crate::workspace_metadata::StoreMetadata::read(root)?.store_id;
    let objects = PackedObjectStore::open(storage_path(root)?)?;
    let content = objects.content_store();
    Ok(LocalRepository {
        store_id,
        objects,
        content,
        heads: LocalPublicationStore::new(root.join("repository.head")),
    })
}

pub fn initialize(root: &Path) -> Result<()> {
    if root.exists() {
        bail!("Wheat store destination already exists: {}", root.display());
    }
    std::fs::create_dir(root)?;
    let heads = LocalPublicationStore::new(root.join("repository.head"));
    let objects = PackedObjectStore::open(root.join("storage"))?;
    let content = objects.content_store();
    runtime()?.block_on(async {
        let publisher = RepositoryPublisher::new(&objects, &content, &heads);
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let closure = publisher.prepare(&repository).await?;
        let labels = wheat_core::ViewLabelManifestRecord::new([("main", base)])
            .map_err(wheat_store::ObjectStoreError::from)?;
        let envelope = publisher
            .prepare_envelope(closure.state.id, &labels)
            .await?;
        publisher.initialize_prepared_envelope(envelope).await
    })?;
    Ok(())
}

pub fn revision_with_edits(
    root: &Path,
    revision: RevisionId,
) -> Result<(
    RevisionRecord,
    Vec<(wheat_core::EditId, wheat_core::EditRecord)>,
)> {
    let objects = PackedObjectStore::open(storage_path(root)?)?;
    read_revision_with_edits(&objects, &objects.content_store(), revision)
}

fn read_revision_with_edits<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    revision_id: RevisionId,
) -> Result<(
    RevisionRecord,
    Vec<(wheat_core::EditId, wheat_core::EditRecord)>,
)> {
    let runtime = runtime()?;
    let reader = RepositoryReader::new(objects, content);
    Ok(runtime.block_on(async {
        let revision = reader.revision(revision_id).await?;
        let mut edits = Vec::with_capacity(revision.edits.len());
        for id in &revision.edits {
            edits.push((*id, reader.edit(*id).await?));
        }
        Ok::<_, wheat_store::RepositoryPublicationError>((revision, edits))
    })?)
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_requires_packed_storage() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("store.json"), "{}").unwrap();
        std::fs::write(root.path().join("repository.head"), "head").unwrap();
        std::fs::create_dir(root.path().join("objects")).unwrap();
        let error = storage_path(root.path()).unwrap_err();
        assert!(error.to_string().contains("invalid Wheat store layout"));

        std::fs::create_dir(root.path().join("storage")).unwrap();
        assert_eq!(
            storage_path(root.path()).unwrap(),
            root.path().join("storage")
        );
    }
}
