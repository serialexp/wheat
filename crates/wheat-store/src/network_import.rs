//! Placing a network from a bundle, or a standalone repository, in a repositories
//! directory.
//!
//! [`RepositoriesDirectory::import_network`] takes a bundle
//! ([`crate::StructureExport`] wrote it) and the names its repositories get here;
//! [`RepositoriesDirectory::adopt_repository`] takes a [`RepositoryDirectory`] (what
//! `wheat import` writes) and gives it a network of its own. Both add the content the
//! heads reach to the directory's content store, copy the structure into a stage,
//! validate every distinct state against the staged structure and that store, install
//! the stage as `networks/<network>` and write the names last.
//!
//! A bundle keeps its network and repository IDs; an adopted repository gets new ones.
//! The network's appearance in `networks/` is the commit point: it is renamed in whole,
//! never over another network, so an installed network is complete and valid. The
//! names follow it, under locks taken first and held to the end. A crash between the
//! two leaves a network no name refers to; running the same import again finds it,
//! checks no name outside the request points into it, and writes the names. Content
//! is only ever added, so what a failed import added is harmless.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    BundleManifestRecord, ContentId, RepositoryLocationRecord, RepositoryStateId, VerifiedContent,
};

use crate::{
    BundleError, CollectionError, ContentSetStore, ContentStore, ContentStoreError,
    NetworkDirectory, ObjectStoreError, PackedObjectStore, PendingRepository, PlacedRepository,
    PublicationError, PublicationStore, RepositoriesDirectory, RepositoriesDirectoryError,
    RepositoryDirectory, RepositoryDirectoryError, RepositoryHead, RepositoryPublicationError,
    RepositoryReader, bundle_content_directory, bundle_structure_directory,
    network_bundle::{COPY_BUFFER_BYTES, MANIFEST, Merged, copy_hashed, put_batch, remove_skipped},
    read_bundle_content_chunk, read_bundle_manifest,
    repositories_directory::{HEAD_FILE, HEADS, NAMES, NETWORKS, read_location, write_location},
    repository_directory::{NameLock, sweep},
    sync_filesystem,
};

/// How an import batches the content it adds, which bounds its memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    /// Content read before it is written: at most this many bytes (one blob may go
    /// over) ...
    pub batch_bytes: usize,
    /// ... and this many blobs. Content IDs are also checked against the store this
    /// many at a time.
    pub batch_blobs: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            batch_bytes: 64 << 20,
            batch_blobs: 4096,
        }
    }
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error(transparent)]
    Repositories(#[from] RepositoriesDirectoryError),
    #[error(transparent)]
    Directory(#[from] RepositoryDirectoryError),
    #[error("import I/O failed")]
    Io(#[from] io::Error),
    #[error("object storage failed")]
    Objects(#[from] ObjectStoreError),
    #[error("content storage failed")]
    Content(#[from] ContentStoreError),
    #[error("reading a head failed")]
    Head(#[from] PublicationError),
    #[error("reading a content set failed")]
    Collection(#[from] CollectionError),
    #[error("a head or state is invalid")]
    Repository(#[from] RepositoryPublicationError),
    #[error("the bundle holds repository {0}, which has no name")]
    Unnamed(Uuid),
    #[error("a name is given for repository {0}, which the bundle does not hold")]
    UnknownRepository(Uuid),
    #[error("{0:?} names two repositories, or one repository has two names")]
    DuplicateName(String),
    #[error("the network is already here, and {0} names one of its repositories")]
    NamedElsewhere(String),
    #[error("the manifest lists {0:?}, which is not a file of a network")]
    InvalidPath(String),
}

/// A network placed in a repositories directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedNetwork {
    pub network: Uuid,
    /// Each repository with the name it was given and its head.
    pub repositories: Vec<(String, PlacedRepository)>,
    /// Whether the network was already there, from an import that stopped before it
    /// wrote the names, and this one only wrote them.
    pub resumed: bool,
}

/// One file to copy into the network.
struct IncomingFile {
    from: PathBuf,
    /// Its path under the network.
    to: PathBuf,
    length: u64,
    /// What the copy must hash to, when something vouches for it.
    digest: Option<[u8; 32]>,
}

struct Incoming {
    network: Uuid,
    repositories: Vec<Uuid>,
    files: Vec<IncomingFile>,
}

/// Where the content to add is read from.
enum Feed<'a> {
    /// The bundle's content list, and the packed store beside it.
    Bundle {
        bundle: &'a Path,
        manifest: &'a BundleManifestRecord,
    },
    /// What the repository's head reaches, in its own store.
    Directory(&'a RepositoryDirectory),
}

/// A name and where it goes.
struct Target<'a> {
    name: &'a str,
    parent: PathBuf,
    repo: &'a std::ffi::OsStr,
    location: RepositoryLocationRecord,
}

impl RepositoriesDirectory {
    /// Places the network the bundle at `bundle` holds, giving each repository the
    /// name `names` pairs with its ID; `content` is this directory's content store
    /// (see [`Self::open_content`]), which gets the bundle's content.
    ///
    /// Every repository needs a name. Importing a network that is here is refused,
    /// unless an earlier import of it stopped before writing the names, or already
    /// wrote the same ones. Reads and writes are blocking: call it from a thread that
    /// may block. The bundle's content store is opened, which writes its lock.
    pub async fn import_network(
        &self,
        bundle: &Path,
        names: &[(Uuid, String)],
        content: &ContentStore,
    ) -> Result<ImportedNetwork, ImportError> {
        self.import_network_with(bundle, names, content, ImportLimits::default())
            .await
    }

    /// [`Self::import_network`] with `limits`.
    ///
    /// # Panics
    /// Panics if a limit is zero.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn import_network_with(
        &self,
        bundle: &Path,
        names: &[(Uuid, String)],
        content: &ContentStore,
        limits: ImportLimits,
    ) -> Result<ImportedNetwork, ImportError> {
        assert!(
            limits.batch_bytes != 0 && limits.batch_blobs != 0,
            "import limits must be nonzero"
        );
        let manifest = read_bundle_manifest(bundle)?;
        let incoming = incoming_bundle(bundle, &manifest)?;
        self.place(
            incoming,
            names,
            content,
            limits,
            Feed::Bundle {
                bundle,
                manifest: &manifest,
            },
        )
        .await
    }

    /// Places the repository in `source` as `name`, in a network of its own with new
    /// IDs; `content` is this directory's content store, which gets the content its
    /// head reaches. `source` must not change meanwhile. Its store is opened, which
    /// writes its lock.
    ///
    /// The copy of the store keeps the content records the original holds, which
    /// nothing reads here; collecting the network's garbage drops them.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn adopt_repository(
        &self,
        source: &RepositoryDirectory,
        name: &str,
        content: &ContentStore,
    ) -> Result<PlacedRepository, ImportError> {
        if !source.is_repository() {
            return Err(
                RepositoryDirectoryError::NotARepository(source.root().to_path_buf()).into(),
            );
        }
        let network = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let mut files = Vec::new();
        for file in source.files()? {
            let to = if file.path == Path::new(HEAD_FILE) {
                Path::new(HEADS)
                    .join(repository.as_simple().to_string())
                    .join(HEAD_FILE)
            } else if file.path.starts_with("storage") {
                file.path.clone()
            } else {
                continue;
            };
            files.push(IncomingFile {
                from: source.root().join(&file.path),
                to,
                length: file.len,
                digest: None,
            });
        }
        let incoming = Incoming {
            network,
            repositories: vec![repository],
            files,
        };
        let placed = self
            .place(
                incoming,
                &[(repository, name.to_owned())],
                content,
                ImportLimits::default(),
                Feed::Directory(source),
            )
            .await?;
        Ok(placed
            .repositories
            .into_iter()
            .next()
            .expect("one repository was placed")
            .1)
    }

    async fn place(
        &self,
        incoming: Incoming,
        names: &[(Uuid, String)],
        content: &ContentStore,
        limits: ImportLimits,
        feed: Feed<'_>,
    ) -> Result<ImportedNetwork, ImportError> {
        let mut targets = self.targets(&incoming, names)?;
        let networks = self.root().join(NETWORKS);
        fs::create_dir_all(&networks)?;
        // Every lock first, held to the end, so nothing else creates, forks or
        // deletes these names meanwhile.
        let mut locks = Vec::with_capacity(targets.len());
        for target in &targets {
            fs::create_dir_all(&target.parent)?;
            locks.push(NameLock::of(&target.parent, target.repo)?);
        }
        for target in &targets {
            crate::repositories_directory::sweep_name_records(&target.parent, target.repo)?;
            sweep(&target.parent, target.repo)?;
            if let Some(existing) = self.live_location(&target.parent, target.repo)?
                && existing != target.location
            {
                return Err(RepositoriesDirectoryError::Exists(target.name.to_owned()).into());
            }
        }

        let directory = self.network(incoming.network);
        let resumed = directory.root().is_dir();
        // Held until the names are written.
        let mut pending: Option<PendingRepository> = None;
        if resumed {
            self.check_resume(&directory, &incoming, &targets)?;
        } else {
            let first = targets.first().expect("an import names a repository");
            let mut staged = PendingRepository::stage(
                locks.remove(0),
                &first.parent,
                first.repo,
                directory.root().to_path_buf(),
            )?;
            let stage = NetworkDirectory::new(staged.directory().root());
            copy_incoming(&incoming.files, stage.root())?;
            for repository in &incoming.repositories {
                if !stage.head(*repository).is_file() {
                    return Err(BundleError::Mismatch(stage.head(*repository)).into());
                }
            }
            add_content(content, &feed, limits).await?;
            validate_stage(&stage, &incoming.repositories, content).await?;
            remove_skipped(&stage.storage(), Path::new("storage"))?;
            sync_filesystem(stage.root())?;
            staged.install()?;
            staged.sync_parent()?;
            pending = Some(staged);
        }

        let mut placed = Vec::with_capacity(targets.len());
        for target in targets.drain(..) {
            if self.live_location(&target.parent, target.repo)? != Some(target.location) {
                write_location(&target.parent, target.repo, target.location)?;
            }
            placed.push((
                target.name.to_owned(),
                PlacedRepository {
                    location: target.location,
                    head: directory.publications(target.location.repository).read()?,
                },
            ));
        }
        drop((pending, locks));
        Ok(ImportedNetwork {
            network: incoming.network,
            repositories: placed,
            resumed,
        })
    }

    /// Where each name goes, sorted by name. Every repository has exactly one name.
    fn targets<'a>(
        &self,
        incoming: &Incoming,
        names: &'a [(Uuid, String)],
    ) -> Result<Vec<Target<'a>>, ImportError> {
        let held = incoming
            .repositories
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut named = BTreeSet::new();
        let mut targets = Vec::with_capacity(names.len());
        for (repository, name) in names {
            if !held.contains(repository) {
                return Err(ImportError::UnknownRepository(*repository));
            }
            if !named.insert(*repository) {
                return Err(ImportError::DuplicateName(name.clone()));
            }
            let (parent, repo) = self.name_path(name)?;
            targets.push(Target {
                name,
                parent,
                repo,
                location: RepositoryLocationRecord {
                    network: incoming.network,
                    repository: *repository,
                },
            });
        }
        if let Some(unnamed) = held.difference(&named).next() {
            return Err(ImportError::Unnamed(*unnamed));
        }
        targets.sort_by(|a, b| a.name.cmp(b.name));
        if let Some(pair) = targets.windows(2).find(|pair| pair[0].name == pair[1].name) {
            return Err(ImportError::DuplicateName(pair[0].name.to_owned()));
        }
        Ok(targets)
    }

    /// Checks a network already here may take the names: it has every head, and no
    /// name outside `targets` points into it.
    fn check_resume(
        &self,
        directory: &NetworkDirectory,
        incoming: &Incoming,
        targets: &[Target<'_>],
    ) -> Result<(), ImportError> {
        for repository in &incoming.repositories {
            if !directory.head(*repository).is_file() {
                return Err(BundleError::Mismatch(directory.head(*repository)).into());
            }
        }
        let ours = targets
            .iter()
            .map(|target| (target.name.to_owned(), target.location))
            .collect::<BTreeMap<_, _>>();
        for (name, location) in self.names_in_network(incoming.network)? {
            if ours.get(&name) != Some(&location) {
                return Err(ImportError::NamedElsewhere(name));
            }
        }
        Ok(())
    }

    /// The names whose repository is in `network`, with where they point. Reads
    /// every name, so it costs the number of repositories the directory holds.
    pub fn names_in_network(
        &self,
        network: Uuid,
    ) -> Result<Vec<(String, RepositoryLocationRecord)>, ImportError> {
        let mut found = Vec::new();
        for owner in fs::read_dir(self.root().join(NAMES))? {
            let owner = owner?;
            let owner_name = owner.file_name().to_string_lossy().into_owned();
            if owner_name.starts_with('.') || !owner.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(owner.path())? {
                let entry = entry?;
                let repo = entry.file_name().to_string_lossy().into_owned();
                if repo.starts_with('.') || !entry.file_type()?.is_file() {
                    continue;
                }
                if let Some(location) = read_location(&entry.path())?
                    && location.network == network
                    && self.network(network).head(location.repository).is_file()
                {
                    found.push((format!("{owner_name}/{repo}"), location));
                }
            }
        }
        Ok(found)
    }
}

/// What the bundle's manifest says to copy, with each path checked: a manifest is
/// input from outside.
fn incoming_bundle(
    bundle: &Path,
    manifest: &BundleManifestRecord,
) -> Result<Incoming, ImportError> {
    let structure = bundle_structure_directory(bundle);
    let heads = manifest
        .repositories
        .iter()
        .map(|repository| {
            (
                Path::new(HEADS)
                    .join(repository.as_simple().to_string())
                    .join(HEAD_FILE),
                *repository,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut files = Vec::with_capacity(manifest.files.len());
    for file in &manifest.files {
        let to = network_path(&file.path, &heads)
            .ok_or_else(|| ImportError::InvalidPath(file.path.clone()))?;
        files.push(IncomingFile {
            from: structure.join(&to),
            to,
            length: file.length,
            digest: Some(file.digest),
        });
    }
    Ok(Incoming {
        network: manifest.network,
        repositories: manifest.repositories.clone(),
        files,
    })
}

/// `path` as a path under the network, when it is a store file or one of `heads`.
fn network_path(path: &str, heads: &BTreeMap<PathBuf, Uuid>) -> Option<PathBuf> {
    if path.is_empty() || path.contains(['\\', '\0']) {
        return None;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return None;
    }
    let relative = parts.iter().collect::<PathBuf>();
    let in_store = parts.len() > 1 && parts[0] == "storage";
    (in_store || heads.contains_key(&relative)).then_some(relative)
}

fn copy_incoming(files: &[IncomingFile], stage: &Path) -> Result<(), ImportError> {
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    for file in files {
        let target = stage.join(&file.to);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let digest = copy_hashed(&file.from, file.length, &target, &mut buffer)?;
        if file.digest.is_some_and(|expected| expected != digest) {
            return Err(BundleError::Mismatch(file.from.clone()).into());
        }
    }
    Ok(())
}

/// Adds to `target` the content `feed` reaches that it lacks.
#[tracing::instrument(level = "debug", skip_all)]
async fn add_content(
    target: &ContentStore,
    feed: &Feed<'_>,
    limits: ImportLimits,
) -> Result<(), ImportError> {
    match feed {
        Feed::Bundle { bundle, manifest } => {
            let store = PackedObjectStore::open(bundle_content_directory(bundle))?;
            let source = store.content_store();
            let mut adder = Adder::new(target, &source, limits);
            let (mut count, mut bytes) = (0_u64, 0_u64);
            for index in 0..manifest.content_chunks.len() {
                for entry in read_bundle_content_chunk(bundle, manifest, index)? {
                    count += 1;
                    bytes += entry.bytes;
                    adder.offer(ContentId::from_digest(entry.id), Some(entry.bytes))?;
                }
            }
            adder.finish()?;
            drop(source);
            store.close()?;
            if (count, bytes) != (manifest.content_count, manifest.content_bytes) {
                return Err(BundleError::Mismatch(bundle.join(MANIFEST)).into());
            }
        }
        Feed::Directory(directory) => {
            let pool = directory.open_store()?;
            let source = pool.content_store();
            let reader = RepositoryReader::new(&pool, &source);
            let head = directory.publications().read()?;
            let state = reader.envelope(head.envelope).await?.state;
            let roots = BTreeSet::from([reader.state(state).await?.required_content]);
            let mut merged = Merged::new(&ContentSetStore::new(&pool), &roots).await?;
            let mut adder = Adder::new(target, &source, limits);
            while let Some(id) = merged.next().await? {
                adder.offer(id, None)?;
            }
            adder.finish()?;
            drop((merged, source));
            pool.close()?;
        }
    }
    Ok(())
}

/// Copies content from `source` to `target` in batches, skipping what `target` holds.
struct Adder<'a> {
    target: &'a ContentStore,
    source: &'a ContentStore,
    limits: ImportLimits,
    offered: Vec<(ContentId, Option<u64>)>,
}

impl<'a> Adder<'a> {
    fn new(target: &'a ContentStore, source: &'a ContentStore, limits: ImportLimits) -> Self {
        Self {
            target,
            source,
            limits,
            offered: Vec::with_capacity(limits.batch_blobs),
        }
    }

    /// Adds `id`, which should be `length` bytes, unless `target` has it.
    fn offer(&mut self, id: ContentId, length: Option<u64>) -> Result<(), ImportError> {
        self.offered.push((id, length));
        if self.offered.len() >= self.limits.batch_blobs {
            self.flush()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), ImportError> {
        self.flush()
    }

    fn flush(&mut self) -> Result<(), ImportError> {
        let ids = self.offered.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let present = self.target.existing_ids(&ids)?;
        let mut batch: Vec<VerifiedContent> = Vec::new();
        let mut batch_bytes = 0;
        for (id, length) in self.offered.drain(..) {
            if present.contains(&id) {
                continue;
            }
            let blob = match self.source.get_verified(id) {
                Ok(blob) => blob,
                Err(ContentStoreError::Missing(_)) => {
                    return Err(BundleError::MissingContent(id).into());
                }
                Err(error) => return Err(error.into()),
            };
            if length.is_some_and(|length| length != blob.len() as u64) {
                return Err(BundleError::Mismatch(PathBuf::from(id.to_string())).into());
            }
            batch_bytes += blob.len();
            batch.push(blob);
            if batch_bytes >= self.limits.batch_bytes {
                put_batch(self.target, &mut batch)?;
                batch_bytes = 0;
            }
        }
        put_batch(self.target, &mut batch)?;
        Ok(())
    }
}

/// Validates every distinct state the staged heads name against the staged store and
/// `content`.
#[tracing::instrument(level = "debug", skip_all)]
async fn validate_stage(
    stage: &NetworkDirectory,
    repositories: &[Uuid],
    content: &ContentStore,
) -> Result<(), ImportError> {
    let pool = PackedObjectStore::open(stage.storage())?;
    let reader = RepositoryReader::new(&pool, content);
    let mut states = BTreeSet::<RepositoryStateId>::new();
    for repository in repositories {
        let head: RepositoryHead = stage.publications(*repository).read()?;
        states.insert(reader.envelope(head.envelope).await?.state);
    }
    for state in states {
        reader.validate_complete(state).await?;
    }
    pool.close()?;
    Ok(())
}
