//! Canonical local repository authority; YAML snapshots are never read or written.
use anyhow::{Context, Result, bail};
use std::path::Path;
use wheat_core::{Repository, Tree};
use wheat_store::{
    ContentStore, LocalPublicationStore, LooseObjectStore, PublicationStore, RepositoryPublisher,
    RepositoryReader,
};

pub fn content_store(root: &Path) -> ContentStore {
    ContentStore::new(root.join(".wheat/content"))
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
    if directory.join("repository.head").exists() {
        bail!("Wheat repository already exists at {}", directory.display());
    }
    let objects = LooseObjectStore::new(directory.join("objects"));
    let content = content_store(root);
    runtime()?.block_on(
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&Repository::new(Tree::new())),
    )?;
    println!("initialized Wheat repository in {}", directory.display());
    Ok(())
}

pub fn with_repository(
    root: &Path,
    persist: bool,
    operation: impl FnOnce(&mut Repository, &mut String) -> Result<()>,
) -> Result<()> {
    reject_legacy(root)?;
    let objects = LooseObjectStore::new(root.join(".wheat/objects"));
    let content = content_store(root);
    let heads = LocalPublicationStore::new(root.join(".wheat/repository.head"));
    let expected = heads
        .read()
        .with_context(|| format!("cannot open Wheat repository: {}", root.display()))?;
    let runtime = runtime()?;
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = runtime.block_on(reader.validate_envelope(expected.envelope))?;
    let mut repository = runtime.block_on(reader.hydrate(envelope.state))?;
    let mut output = String::new();
    operation(&mut repository, &mut output)?;
    if persist {
        runtime.block_on(
            RepositoryPublisher::new(&objects, &content, &heads).publish(expected, &repository),
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
    use wheat_core::Transformation;

    #[test]
    fn concurrent_writer_cannot_overwrite_winning_head() {
        let dir = tempfile::tempdir().unwrap();
        initialize(dir.path()).unwrap();
        let result = with_repository(dir.path(), true, |stale, output| {
            stale.create_change([], Transformation::default())?;
            output.push_str("must not be printed\n");
            with_repository(dir.path(), true, |winner, _| {
                winner.create_change([], Transformation::default())?;
                Ok(())
            })?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("publication expected"));
        with_repository(dir.path(), false, |repository, _| {
            assert_eq!(repository.current_revisions().count(), 1);
            Ok(())
        })
        .unwrap();
    }
}
