//! Canonical local repository authority; YAML snapshots are never read or written.
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::path::Path;
use wheat_core::{ObjectId, Repository, RevisionId, RevisionRecord, Tree};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore, ObjectStoreError,
    PackedObjectStore, RepositoryPublisher, RepositoryReader, StoredObject,
};

pub(crate) enum LocalObjects {
    Loose(LooseObjectStore),
    Packed(PackedObjectStore),
}

#[async_trait]
impl ImmutableObjectStore for LocalObjects {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        match self {
            Self::Loose(store) => store.put_if_absent(id, bytes).await,
            Self::Packed(store) => store.put_if_absent(id, bytes).await,
        }
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        match self {
            Self::Loose(store) => store.put_batch(objects, concurrency).await,
            Self::Packed(store) => store.put_batch(objects, concurrency).await,
        }
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        match self {
            Self::Loose(store) => store.get(id).await,
            Self::Packed(store) => store.get(id).await,
        }
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        match self {
            Self::Loose(store) => store.exists_many(ids).await,
            Self::Packed(store) => store.exists_many(ids).await,
        }
    }
}

pub(crate) struct LocalRepository {
    pub store_id: uuid::Uuid,
    pub objects: LocalObjects,
    pub content: ContentStore,
    pub heads: LocalPublicationStore,
}

pub fn content_store(root: &Path) -> Result<ContentStore> {
    match repository_layout(root)? {
        RepositoryLayout::Loose => Ok(ContentStore::new(root.join("content"))),
        RepositoryLayout::Packed => {
            Ok(PackedObjectStore::open(root.join("storage"))?.content_store())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryLayout {
    Loose,
    Packed,
}

pub(crate) fn require_packed_layout(root: &Path) -> Result<()> {
    match repository_layout(root)? {
        RepositoryLayout::Packed => Ok(()),
        RepositoryLayout::Loose => bail!(
            "repack requires a packed Wheat store; {} uses the loose store layout",
            root.display()
        ),
    }
}

fn repository_layout(root: &Path) -> Result<RepositoryLayout> {
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
    let head = root.join("repository.head");
    match (
        head.is_file(),
        root.join("objects").is_dir(),
        root.join("storage").is_dir(),
    ) {
        (true, true, false) => Ok(RepositoryLayout::Loose),
        (true, false, true) => Ok(RepositoryLayout::Packed),
        _ => bail!("invalid Wheat store layout at {}", root.display()),
    }
}

pub(crate) fn projection_path(root: &Path) -> Result<std::path::PathBuf> {
    repository_layout(root)?;
    Ok(root.join("projections/git.git"))
}

pub(crate) fn open(root: &Path) -> Result<LocalRepository> {
    let store_id = crate::workspace_metadata::StoreMetadata::read(root)?.store_id;
    match repository_layout(root)? {
        RepositoryLayout::Loose => Ok(LocalRepository {
            store_id,
            objects: LocalObjects::Loose(LooseObjectStore::new(root.join("objects"))),
            content: ContentStore::new(root.join("content")),
            heads: LocalPublicationStore::new(root.join("repository.head")),
        }),
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            let content = objects.content_store();
            Ok(LocalRepository {
                store_id,
                objects: LocalObjects::Packed(objects),
                content,
                heads: LocalPublicationStore::new(root.join("repository.head")),
            })
        }
    }
}

pub fn initialize(root: &Path) -> Result<()> {
    if root.exists() {
        bail!("Wheat store destination already exists: {}", root.display());
    }
    std::fs::create_dir(root)?;
    let heads = LocalPublicationStore::new(root.join("repository.head"));
    let objects = LooseObjectStore::new(root.join("objects"));
    let content = ContentStore::new(root.join("content"));
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
    match repository_layout(root)? {
        RepositoryLayout::Loose => read_revision_with_edits(
            &LooseObjectStore::new(root.join("objects")),
            &ContentStore::new(root.join("content")),
            revision,
        ),
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            read_revision_with_edits(&objects, &objects.content_store(), revision)
        }
    }
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

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_store(root: &Path) {
        std::fs::write(root.join("store.json"), "{}").unwrap();
        std::fs::write(root.join("repository.head"), "head").unwrap();
    }

    #[test]
    fn repack_layout_check_accepts_only_packed_stores() {
        let packed = tempfile::tempdir().unwrap();
        base_store(packed.path());
        std::fs::create_dir(packed.path().join("storage")).unwrap();
        require_packed_layout(packed.path()).unwrap();

        let loose = tempfile::tempdir().unwrap();
        base_store(loose.path());
        std::fs::create_dir(loose.path().join("objects")).unwrap();
        let error = require_packed_layout(loose.path()).unwrap_err();
        assert!(error.to_string().contains("requires a packed Wheat store"));
    }
}
