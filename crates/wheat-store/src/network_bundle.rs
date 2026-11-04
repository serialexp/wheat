//! A fork network exported as a bundle: its structure apart from its content.
//!
//! ```text
//! <bundle>/
//!   bundle.manifest                      BundleManifestV1, written last
//!   structure/storage/                   the network's packed store, as copied
//!   structure/heads/<repository>/repository.head
//!   content.list/<n>                     BundleContentChunkV1: the content IDs the
//!                                        heads reach, in order, with their lengths
//!   content/                             a packed store holding that content
//! ```
//!
//! A host whose content is already shared (Gothab's blob store) moves only the
//! structure. Exporting takes two steps so a server can hold the network still for the
//! first only: [`RepositoriesDirectory::export_network`] copies the structure, which
//! must not change meanwhile, and [`StructureExport::finish`] lists and copies the
//! content (immutable, so nothing is held for it), validates every head against the
//! copy, and installs the bundle. The bundle is built in a hidden stage beside its
//! destination and renamed into place, so one with a manifest is complete.

use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;
use wheat_core::{
    BundleFileRecord, BundleManifestRecord, CHUNKED_CONTENT_THRESHOLD, ContentId, ContentSetId,
    InventoryRecord, RepositoryStateId, StoreRecordError, VerifiedContent,
    decode_bundle_content_chunk, decode_bundle_manifest, encode_bundle_content_chunk,
    encode_bundle_manifest,
};

use crate::{
    CollectionError, ContentSetCursor, ContentSetStore, ContentStore, ContentStoreError,
    LocalPublicationStore, ObjectStoreError, PackedObjectStore, PendingRepository,
    PublicationError, PublicationStore, RepositoriesDirectory, RepositoryDirectory,
    RepositoryDirectoryError, RepositoryHead, RepositoryPublicationError, RepositoryReader,
    packed::files::{StoreFile, classify},
    sync_filesystem,
};

pub(crate) const MANIFEST: &str = "bundle.manifest";
const STRUCTURE: &str = "structure";
const CONTENT_LIST: &str = "content.list";
const CONTENT: &str = "content";
pub(crate) const COPY_BUFFER_BYTES: usize = 1 << 20;
/// Writers for a batch of content.
pub(crate) const WRITE_CONCURRENCY: usize = 8;

/// How [`StructureExport::finish_with`] batches its work, which bounds its memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportLimits {
    /// Content IDs in a chunk of the list.
    pub chunk_entries: usize,
    /// Content read before it is written to the bundle: at most this many bytes (one
    /// blob may go over) ...
    pub batch_bytes: usize,
    /// ... and this many blobs.
    pub batch_blobs: usize,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            // 640 KiB of list entries.
            chunk_entries: 16_384,
            batch_bytes: 64 << 20,
            batch_blobs: 4096,
        }
    }
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("there is no network {0}")]
    Missing(Uuid),
    #[error(transparent)]
    Directory(#[from] RepositoryDirectoryError),
    #[error("bundle I/O failed")]
    Io(#[from] io::Error),
    #[error("bundle record is invalid")]
    Record(#[from] StoreRecordError),
    #[error("reading a head failed")]
    Head(#[from] PublicationError),
    #[error("object storage failed")]
    Objects(#[from] ObjectStoreError),
    #[error("content storage failed")]
    Content(#[from] ContentStoreError),
    #[error("reading a content set failed")]
    Collection(#[from] CollectionError),
    #[error("a head is invalid")]
    Repository(#[from] RepositoryPublicationError),
    #[error("a head reaches content {0} that the source does not hold")]
    MissingContent(ContentId),
    #[error("a content set lists {0} out of order")]
    ContentOrder(ContentId),
    #[error("{} changed after it was copied", .0.display())]
    Changed(PathBuf),
    #[error("{} is not what the manifest lists", .0.display())]
    Mismatch(PathBuf),
}

impl RepositoriesDirectory {
    /// Starts exporting `network` to the new directory `destination`: copies its
    /// structure (store files and every head) into a stage. The network must not
    /// change until this returns; `Registry::quiesce` holds it in a server, and the
    /// hold can end then.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn export_network(
        &self,
        network: Uuid,
        destination: impl Into<PathBuf>,
    ) -> Result<StructureExport, BundleError> {
        let directory = self.network(network);
        if !directory.root().is_dir() {
            return Err(BundleError::Missing(network));
        }
        let pending = RepositoryDirectory::new(destination).begin_create()?;
        let structure = pending.directory().root().join(STRUCTURE);
        let repositories = directory.repositories()?;
        let mut files = Vec::new();
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        for file in directory.files()? {
            let target = structure.join(&file.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let digest = copy_hashed(
                &directory.root().join(&file.path),
                file.len,
                &target,
                &mut buffer,
            )?;
            files.push(BundleFileRecord {
                path: portable(&file.path)?,
                length: file.len,
                digest,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(StructureExport {
            pending,
            network,
            repositories,
            files,
        })
    }
}

/// A bundle whose structure is copied; see [`RepositoriesDirectory::export_network`].
/// Dropping it removes the stage.
#[derive(Debug)]
pub struct StructureExport {
    pending: PendingRepository,
    network: Uuid,
    repositories: Vec<Uuid>,
    files: Vec<BundleFileRecord>,
}

impl StructureExport {
    /// The structure's files, with the digests of the bytes copied.
    #[must_use]
    pub fn files(&self) -> &[BundleFileRecord] {
        &self.files
    }

    /// Lists the content the copied heads reach, copies it from `content` (where the
    /// network's content is read from), validates every distinct state against the
    /// copy, writes the manifest and installs the bundle. Returns the manifest.
    ///
    /// It reads only the stage and `content`, which never changes once written, so a
    /// server need not hold the network. Reads and writes are blocking: call it from
    /// a thread that may block.
    pub async fn finish(self, content: &ContentStore) -> Result<BundleManifestRecord, BundleError> {
        self.finish_with(content, ExportLimits::default()).await
    }

    /// [`Self::finish`] with `limits`.
    ///
    /// # Panics
    /// Panics if a limit is zero.
    pub async fn finish_with(
        self,
        content: &ContentStore,
        limits: ExportLimits,
    ) -> Result<BundleManifestRecord, BundleError> {
        assert!(
            limits.chunk_entries != 0 && limits.batch_bytes != 0 && limits.batch_blobs != 0,
            "export limits must be nonzero"
        );
        let Self {
            mut pending,
            network,
            repositories,
            files,
        } = self;
        let stage = pending.directory().root().to_path_buf();
        let structure = stage.join(STRUCTURE);
        let listing = copy_content(&stage, &structure, &repositories, content, limits).await?;

        validate(&stage, &structure, &listing.states).await?;

        // Opening the stores left files only a running store needs; they are not part
        // of the bundle, and what was listed must still be what is there.
        remove_skipped(&structure.join("storage"), Path::new("storage"))?;
        remove_skipped(&stage.join(CONTENT), Path::new(""))?;
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        for file in &files {
            let path = structure.join(&file.path);
            if hash_file(&path, &mut buffer)? != (file.length, file.digest) {
                return Err(BundleError::Changed(PathBuf::from(&file.path)));
            }
        }

        let manifest = BundleManifestRecord {
            network,
            repositories,
            files,
            content_count: listing.count,
            content_bytes: listing.bytes,
            content_chunks: listing.chunks,
        };
        write_new(&stage.join(MANIFEST), &encode_bundle_manifest(&manifest)?)?;
        sync_filesystem(&stage)?;
        pending.install()?;
        pending.sync_parent()?;
        Ok(manifest)
    }
}

/// What listing and copying the content found.
struct Listing {
    states: BTreeSet<RepositoryStateId>,
    count: u64,
    bytes: u64,
    chunks: Vec<[u8; 32]>,
}

/// Writes `content.list` and the bundle's content store: every blob any copied head's
/// state lists in `required_content`, once, in ID order.
#[tracing::instrument(level = "debug", skip_all)]
async fn copy_content(
    stage: &Path,
    structure: &Path,
    repositories: &[Uuid],
    source: &ContentStore,
    limits: ExportLimits,
) -> Result<Listing, BundleError> {
    let pool = PackedObjectStore::open(structure.join("storage"))?;
    let pool_content = pool.content_store();
    let reader = RepositoryReader::new(&pool, &pool_content);
    let mut states = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for repository in repositories {
        let head = read_head(structure, *repository)?;
        let state = reader.envelope(head.envelope).await?.state;
        if states.insert(state) {
            roots.insert(reader.state(state).await?.required_content);
        }
    }
    let mut merged = Merged::new(&ContentSetStore::new(&pool), &roots).await?;

    fs::create_dir(stage.join(CONTENT_LIST))?;
    let bundle = PackedObjectStore::open_deferred(stage.join(CONTENT))?;
    let target = bundle.content_store();
    let mut listing = Listing {
        states,
        count: 0,
        bytes: 0,
        chunks: Vec::new(),
    };
    let mut chunk = Vec::with_capacity(limits.chunk_entries);
    let mut batch: Vec<VerifiedContent> = Vec::new();
    let mut batch_bytes = 0;
    while let Some(id) = merged.next().await? {
        let missing = |error| match error {
            ContentStoreError::Missing(_) => BundleError::MissingContent(id),
            error => error.into(),
        };
        // A long blob is copied a chunk at a time rather than batched whole.
        let length = if source.logical_length(id).map_err(missing)? > CHUNKED_CONTENT_THRESHOLD {
            source.copy_to(id, &target).map_err(missing)?
        } else {
            let blob = source.get_verified(id).map_err(missing)?;
            let length = blob.len();
            batch_bytes += length;
            batch.push(blob);
            if batch_bytes >= limits.batch_bytes || batch.len() >= limits.batch_blobs {
                put_batch(&target, &mut batch)?;
                batch_bytes = 0;
            }
            length as u64
        };
        chunk.push(InventoryRecord {
            id: *id.as_bytes(),
            bytes: length,
        });
        listing.count += 1;
        listing.bytes += length;
        if chunk.len() == limits.chunk_entries {
            listing
                .chunks
                .push(write_chunk(stage, listing.chunks.len(), &chunk)?);
            chunk.clear();
        }
    }
    put_batch(&target, &mut batch)?;
    if !chunk.is_empty() {
        listing
            .chunks
            .push(write_chunk(stage, listing.chunks.len(), &chunk)?);
    }
    drop((target, pool_content));
    // Closing a deferred store publishes what it wrote ahead into its segments.
    bundle.close()?;
    pool.close()?;
    Ok(listing)
}

pub(crate) fn put_batch(
    target: &ContentStore,
    batch: &mut Vec<VerifiedContent>,
) -> Result<(), BundleError> {
    let blobs = batch
        .iter()
        .map(VerifiedContent::borrowed)
        .collect::<Vec<_>>();
    target.put_many(&blobs, WRITE_CONCURRENCY)?;
    batch.clear();
    Ok(())
}

fn write_chunk(
    stage: &Path,
    index: usize,
    chunk: &[InventoryRecord],
) -> Result<[u8; 32], BundleError> {
    let bytes = encode_bundle_content_chunk(chunk.iter().copied())?;
    write_new(
        &stage.join(CONTENT_LIST).join(format!("{index:06}")),
        &bytes,
    )?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

/// Validates each of `states` against the staged structure and the staged content.
#[tracing::instrument(level = "debug", skip_all)]
async fn validate(
    stage: &Path,
    structure: &Path,
    states: &BTreeSet<RepositoryStateId>,
) -> Result<(), BundleError> {
    let pool = PackedObjectStore::open(structure.join("storage"))?;
    let content = PackedObjectStore::open(stage.join(CONTENT))?;
    let store = content.content_store();
    let reader = RepositoryReader::new(&pool, &store);
    for state in states {
        reader.validate_complete(*state).await?;
    }
    drop(store);
    content.close()?;
    pool.close()?;
    Ok(())
}

fn read_head(structure: &Path, repository: Uuid) -> Result<RepositoryHead, BundleError> {
    let path = structure
        .join("heads")
        .join(repository.as_simple().to_string())
        .join("repository.head");
    Ok(LocalPublicationStore::new(path).read()?)
}

/// The sorted union of content sets, read through one cursor each.
pub(crate) struct Merged<'a> {
    cursors: Vec<ContentSetCursor<'a, PackedObjectStore>>,
    next: BinaryHeap<Reverse<(ContentId, usize)>>,
    last: Option<ContentId>,
}

impl<'a> Merged<'a> {
    pub(crate) async fn new(
        sets: &ContentSetStore<'a, PackedObjectStore>,
        roots: &BTreeSet<ContentSetId>,
    ) -> Result<Self, BundleError> {
        let mut merged = Self {
            cursors: Vec::with_capacity(roots.len()),
            next: BinaryHeap::with_capacity(roots.len()),
            last: None,
        };
        for root in roots {
            let mut cursor = sets.cursor(*root).await?;
            if let Some(first) = cursor.next().await? {
                merged.next.push(Reverse((first, merged.cursors.len())));
            }
            merged.cursors.push(cursor);
        }
        Ok(merged)
    }

    /// The next content ID not yet returned; an ID two sets hold comes once. A set
    /// that does not list its IDs in order is refused.
    pub(crate) async fn next(&mut self) -> Result<Option<ContentId>, BundleError> {
        while let Some(Reverse((id, index))) = self.next.pop() {
            if let Some(following) = self.cursors[index].next().await? {
                if following <= id {
                    return Err(BundleError::ContentOrder(following));
                }
                self.next.push(Reverse((following, index)));
            }
            if self.last != Some(id) {
                self.last = Some(id);
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

/// The manifest of the bundle at `bundle`.
pub fn read_bundle_manifest(bundle: &Path) -> Result<BundleManifestRecord, BundleError> {
    Ok(decode_bundle_manifest(&fs::read(bundle.join(MANIFEST))?)?)
}

/// Chunk `index` of the content list of the bundle at `bundle`, which `manifest`
/// describes: its bytes must be the digest the manifest names.
pub fn read_bundle_content_chunk(
    bundle: &Path,
    manifest: &BundleManifestRecord,
    index: usize,
) -> Result<Vec<InventoryRecord>, BundleError> {
    let path = bundle.join(CONTENT_LIST).join(format!("{index:06}"));
    let bytes = fs::read(&path)?;
    if manifest.content_chunks.get(index) != Some(blake3::hash(&bytes).as_bytes()) {
        return Err(BundleError::Mismatch(path));
    }
    Ok(decode_bundle_content_chunk(&bytes)?)
}

/// The bundle's packed content store, to be opened by whoever reads the content.
#[must_use]
pub fn bundle_content_directory(bundle: &Path) -> PathBuf {
    bundle.join(CONTENT)
}

/// The directory holding the bundle's structure files.
#[must_use]
pub fn bundle_structure_directory(bundle: &Path) -> PathBuf {
    bundle.join(STRUCTURE)
}

/// `path` with `/` separators, as the manifest keeps it.
fn portable(path: &Path) -> Result<String, BundleError> {
    path.components()
        .map(|component| {
            component.as_os_str().to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "a store path is not UTF-8").into()
            })
        })
        .collect::<Result<Vec<_>, BundleError>>()
        .map(|parts| parts.join("/"))
}

/// Copies `len` bytes of `source` to the new file `target`, returning their digest.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn copy_hashed(
    source: &Path,
    len: u64,
    target: &Path,
    buffer: &mut [u8],
) -> Result<[u8; 32], BundleError> {
    let mut reader = File::open(source)?.take(len);
    let mut writer = File::create_new(target)?;
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0_u64;
    loop {
        let read = reader.read(buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
    if copied != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{} shrank while it was copied", source.display()),
        )
        .into());
    }
    Ok(*hasher.finalize().as_bytes())
}

/// The length and digest of the file at `path`.
fn hash_file(path: &Path, buffer: &mut [u8]) -> Result<(u64, [u8; 32]), BundleError> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut length = 0_u64;
    loop {
        let read = file.read(buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length += read as u64;
    }
    Ok((length, *hasher.finalize().as_bytes()))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), BundleError> {
    let mut file = File::create_new(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Removes the files under `directory` (at `relative` in its store) that a copy of
/// the store leaves out: the lock and the lookup index a running store keeps.
pub(crate) fn remove_skipped(directory: &Path, relative: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = relative.join(entry.file_name());
        if file_type.is_dir() {
            remove_skipped(&entry.path(), &path)?;
        } else if file_type.is_file() {
            let in_store = path.strip_prefix("storage").unwrap_or(&path);
            if matches!(classify(in_store), StoreFile::Skipped) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}
