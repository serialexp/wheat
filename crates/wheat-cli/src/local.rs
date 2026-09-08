//! Canonical local repository authority; YAML snapshots are never read or written.
use anyhow::{Context, Result, bail};
use std::path::Path;
use wheat_core::{ChangeId, Conflict, Repository, RevisionId, RevisionRecord, Tree, ViewId};
use wheat_store::{
    ConflictSetStore, ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    PackedObjectStore, PublicationStore, RepositoryPublisher, RepositoryReader, RevisionMapStore,
};

pub struct RepositorySummary {
    pub working_view: ViewId,
    pub published_view: ViewId,
    pub changes: usize,
    pub conflicts: usize,
}

pub fn content_store(root: &Path) -> Result<ContentStore> {
    match repository_layout(root)? {
        RepositoryLayout::Loose => Ok(ContentStore::new(root.join(".wheat/content"))),
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

fn repository_layout(root: &Path) -> Result<RepositoryLayout> {
    let loose_head = root.join(".wheat/repository.head");
    let packed_head = root.join("repository.head");
    match (loose_head.is_file(), packed_head.is_file()) {
        (true, false) => Ok(RepositoryLayout::Loose),
        (false, true) if root.join("storage").is_dir() => Ok(RepositoryLayout::Packed),
        (true, true) => bail!(
            "ambiguous Wheat repository at {}: both loose and packed heads exist",
            root.display()
        ),
        _ => bail!("cannot open Wheat repository: {}", root.display()),
    }
}

fn reject_legacy(root: &Path) -> Result<()> {
    if root.join(".wheat/repository.yaml").exists() {
        bail!(
            "legacy YAML Wheat repository detected; prototype formats are incompatible. Preserve it separately and initialize a fresh repository"
        );
    }
    Ok(())
}

pub fn initialize(root: &Path) -> Result<()> {
    reject_legacy(root)?;
    let directory = root.join(".wheat");
    let heads = LocalPublicationStore::new(directory.join("repository.head"));
    if directory.join("repository.head").exists() || root.join("repository.head").exists() {
        bail!("Wheat repository already exists at {}", root.display());
    }
    let objects = LooseObjectStore::new(directory.join("objects"));
    let content = ContentStore::new(directory.join("content"));
    runtime()?.block_on(
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&Repository::new(Tree::new())),
    )?;
    println!("initialized Wheat repository in {}", directory.display());
    Ok(())
}

pub fn repository_summary(root: &Path) -> Result<RepositorySummary> {
    reject_legacy(root)?;
    match repository_layout(root)? {
        RepositoryLayout::Loose => {
            let objects = LooseObjectStore::new(root.join(".wheat/objects"));
            let content = ContentStore::new(root.join(".wheat/content"));
            let heads = LocalPublicationStore::new(root.join(".wheat/repository.head"));
            summarize(root, &objects, &content, &heads)
        }
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            let content = objects.content_store();
            let heads = LocalPublicationStore::new(root.join("repository.head"));
            summarize(root, &objects, &content, &heads)
        }
    }
}

fn summarize<O: ImmutableObjectStore>(
    root: &Path,
    objects: &O,
    content: &ContentStore,
    heads: &LocalPublicationStore,
) -> Result<RepositorySummary> {
    let expected = heads
        .read()
        .with_context(|| format!("cannot open Wheat repository: {}", root.display()))?;
    let runtime = runtime()?;
    let reader = RepositoryReader::new(objects, content);
    let envelope = runtime.block_on(reader.envelope(expected.envelope))?;
    let state = runtime.block_on(reader.state(envelope.state))?;
    let working = runtime.block_on(reader.view(state.working_view))?;
    let revision_store = RevisionMapStore::new(objects);
    let conflict_store = ConflictSetStore::new(objects);
    let (changes, conflicts) = runtime.block_on(async {
        let mut change_count = 0;
        let mut revisions = revision_store.cursor(working.revisions).await?;
        while revisions.next().await?.is_some() {
            change_count += 1;
        }
        let mut conflict_count = 0;
        let mut conflicts = conflict_store.cursor(working.unresolved_conflicts).await?;
        while conflicts.next().await?.is_some() {
            conflict_count += 1;
        }
        Ok::<_, wheat_store::RepositoryPublicationError>((change_count, conflict_count))
    })?;
    Ok(RepositorySummary {
        working_view: state.working_view,
        published_view: state.published_view,
        changes,
        conflicts,
    })
}

pub fn revision_with_edits(
    root: &Path,
    revision: RevisionId,
) -> Result<(
    RevisionRecord,
    Vec<(wheat_core::EditId, wheat_core::EditRecord)>,
)> {
    reject_legacy(root)?;
    match repository_layout(root)? {
        RepositoryLayout::Loose => read_revision_with_edits(
            &LooseObjectStore::new(root.join(".wheat/objects")),
            &ContentStore::new(root.join(".wheat/content")),
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

pub fn current_conflicts(root: &Path) -> Result<Vec<Conflict>> {
    reject_legacy(root)?;
    match repository_layout(root)? {
        RepositoryLayout::Loose => read_current_conflicts(
            root,
            &LooseObjectStore::new(root.join(".wheat/objects")),
            &ContentStore::new(root.join(".wheat/content")),
            &LocalPublicationStore::new(root.join(".wheat/repository.head")),
        ),
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            read_current_conflicts(
                root,
                &objects,
                &objects.content_store(),
                &LocalPublicationStore::new(root.join("repository.head")),
            )
        }
    }
}

fn read_current_conflicts<O: ImmutableObjectStore>(
    root: &Path,
    objects: &O,
    content: &ContentStore,
    heads: &LocalPublicationStore,
) -> Result<Vec<Conflict>> {
    let runtime = runtime()?;
    let reader = RepositoryReader::new(objects, content);
    let expected = heads
        .read()
        .with_context(|| format!("cannot open Wheat repository: {}", root.display()))?;
    let envelope = runtime.block_on(reader.envelope(expected.envelope))?;
    let state = runtime.block_on(reader.state(envelope.state))?;
    let working = runtime.block_on(reader.view(state.working_view))?;
    let conflicts = ConflictSetStore::new(objects);
    Ok(runtime.block_on(async {
        let mut result = Vec::new();
        let mut cursor = conflicts.cursor(working.unresolved_conflicts).await?;
        while let Some(id) = cursor.next().await? {
            result.push(reader.conflict(id).await?);
        }
        Ok::<_, wheat_store::RepositoryPublicationError>(result)
    })?)
}

pub fn current_revisions(root: &Path) -> Result<Vec<(ChangeId, RevisionId)>> {
    reject_legacy(root)?;
    match repository_layout(root)? {
        RepositoryLayout::Loose => {
            let objects = LooseObjectStore::new(root.join(".wheat/objects"));
            let content = ContentStore::new(root.join(".wheat/content"));
            let heads = LocalPublicationStore::new(root.join(".wheat/repository.head"));
            read_current_revisions(root, &objects, &content, &heads)
        }
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            let content = objects.content_store();
            let heads = LocalPublicationStore::new(root.join("repository.head"));
            read_current_revisions(root, &objects, &content, &heads)
        }
    }
}

fn read_current_revisions<O: ImmutableObjectStore>(
    root: &Path,
    objects: &O,
    content: &ContentStore,
    heads: &LocalPublicationStore,
) -> Result<Vec<(ChangeId, RevisionId)>> {
    let expected = heads
        .read()
        .with_context(|| format!("cannot open Wheat repository: {}", root.display()))?;
    let runtime = runtime()?;
    let reader = RepositoryReader::new(objects, content);
    let envelope = runtime.block_on(reader.envelope(expected.envelope))?;
    let state = runtime.block_on(reader.state(envelope.state))?;
    let working = runtime.block_on(reader.view(state.working_view))?;
    let revisions = RevisionMapStore::new(objects);
    Ok(runtime.block_on(async {
        let mut result = Vec::new();
        let mut cursor = revisions.cursor(working.revisions).await?;
        while let Some(revision) = cursor.next().await? {
            result.push(revision);
        }
        Ok::<_, wheat_store::RepositoryPublicationError>(result)
    })?)
}

pub fn with_repository(
    root: &Path,
    persist: bool,
    operation: impl FnOnce(&mut Repository, &ContentStore, &mut String) -> Result<()>,
) -> Result<()> {
    reject_legacy(root)?;
    match repository_layout(root)? {
        RepositoryLayout::Loose => {
            let objects = LooseObjectStore::new(root.join(".wheat/objects"));
            let content = ContentStore::new(root.join(".wheat/content"));
            let heads = LocalPublicationStore::new(root.join(".wheat/repository.head"));
            operate(root, persist, operation, &objects, &content, &heads)
        }
        RepositoryLayout::Packed => {
            let objects = PackedObjectStore::open(root.join("storage"))?;
            let content = objects.content_store();
            let heads = LocalPublicationStore::new(root.join("repository.head"));
            operate(root, persist, operation, &objects, &content, &heads)
        }
    }
}

fn operate<O: ImmutableObjectStore>(
    root: &Path,
    persist: bool,
    operation: impl FnOnce(&mut Repository, &ContentStore, &mut String) -> Result<()>,
    objects: &O,
    content: &ContentStore,
    heads: &LocalPublicationStore,
) -> Result<()> {
    let expected = heads
        .read()
        .with_context(|| format!("cannot open Wheat repository: {}", root.display()))?;
    let runtime = runtime()?;
    let reader = RepositoryReader::new(objects, content);
    let envelope = runtime.block_on(reader.validate_envelope(expected.envelope))?;
    let mut repository = runtime.block_on(reader.hydrate(envelope.state))?;
    let mut output = String::new();
    operation(&mut repository, content, &mut output)?;
    if persist {
        let published = repository.published_view().id;
        repository.publish(published)?;
        runtime.block_on(
            RepositoryPublisher::new(objects, content, heads).publish(expected, &repository),
        )?;
    }
    print!("{output}");
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_core::{ApplicationContext, RevisionEdits};

    #[test]
    fn concurrent_writer_cannot_overwrite_winning_head() {
        let dir = tempfile::tempdir().unwrap();
        initialize(dir.path()).unwrap();
        let result = with_repository(dir.path(), true, |stale, content, output| {
            stale.create_change(
                [],
                RevisionEdits::default(),
                ApplicationContext::new(content),
            )?;
            output.push_str("must not be printed\n");
            with_repository(dir.path(), true, |winner, content, _| {
                winner.create_change(
                    [],
                    RevisionEdits::default(),
                    ApplicationContext::new(content),
                )?;
                Ok(())
            })?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("publication expected"));
        with_repository(dir.path(), false, |repository, _, _| {
            assert_eq!(repository.current_revisions().count(), 1);
            Ok(())
        })
        .unwrap();
    }
}
