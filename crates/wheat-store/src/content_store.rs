//! Content retention facade and loose reference backend.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use thiserror::Error;
use wheat_core::ContentId;

use crate::Durability;

#[derive(Debug, Error)]
pub enum ContentStoreError {
    #[error("content {0} is missing")]
    Missing(ContentId),
    #[error("content {expected} failed integrity verification (found {actual})")]
    Corrupt {
        expected: ContentId,
        actual: ContentId,
    },
    #[error("content {id} changed while it was being read")]
    Changed { id: ContentId },
    #[error("content {id} is at least {minimum} bytes, exceeding the read limit of {max} bytes")]
    TooLarge {
        id: ContentId,
        max: usize,
        /// A trustworthy lower bound derived before rejecting the read.
        minimum: usize,
    },
    #[error("content store I/O failed")]
    Io(#[from] io::Error),
}

pub(crate) trait ContentBackend: std::fmt::Debug + Send + Sync {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError>;
    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError>;

    /// List all content IDs stored in this backend.
    ///
    /// Returns `None` if the backend does not support listing (the caller
    /// should fall back to per-item `get_bounded` existence checks).
    /// Remote backends override this to use a single LIST/scan call
    /// instead of one GET per item.
    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        None
    }

    /// Fetch multiple content blobs concurrently.
    ///
    /// Returns `None` if the backend does not support concurrent reads
    /// (the caller should fall back to sequential `get_bounded` calls).
    /// Remote backends override this to issue concurrent S3 GETs.
    #[allow(clippy::type_complexity)]
    fn get_many_bounded(
        &self,
        _ids: &[ContentId],
        _max: usize,
    ) -> Option<Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError>> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
    backend: Option<Arc<dyn ContentBackend>>,
    durability: Durability,
}

impl ContentStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_durability(root, Durability::Durable)
    }

    #[must_use]
    pub fn with_durability(root: impl Into<PathBuf>, durability: Durability) -> Self {
        Self {
            root: root.into(),
            backend: None,
            durability,
        }
    }

    pub(crate) fn with_backend(
        root: PathBuf,
        backend: Arc<dyn ContentBackend>,
        durability: Durability,
    ) -> Self {
        Self {
            root,
            backend: Some(backend),
            durability,
        }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ContentId, ContentStoreError> {
        let id = ContentId::of(bytes);
        self.put_with_id(id, bytes)?;
        Ok(id)
    }

    /// Retains already-addressed content with bounded parallel durability work.
    /// Every claimed ID is verified before publication.
    pub fn put_many(
        &self,
        contents: impl IntoIterator<Item = (ContentId, Vec<u8>)>,
        concurrency: usize,
    ) -> Result<(), ContentStoreError> {
        let mut deduplicated = std::collections::BTreeMap::new();
        for (id, bytes) in contents {
            let actual = ContentId::of(&bytes);
            if actual != id {
                return Err(ContentStoreError::Corrupt {
                    expected: id,
                    actual,
                });
            }
            if let Some(previous) = deduplicated.get(&id) {
                if previous != &bytes {
                    return Err(ContentStoreError::Corrupt {
                        expected: id,
                        actual,
                    });
                }
            } else {
                deduplicated.insert(id, bytes);
            }
        }
        if deduplicated.is_empty() {
            return Ok(());
        }
        if let Some(backend) = &self.backend {
            return backend.put_many(&deduplicated.into_iter().collect::<Vec<_>>());
        }
        let workers = concurrency.max(1).min(deduplicated.len());
        let (sender, receiver) = mpsc::channel();
        for content in deduplicated {
            sender.send(content).expect("receiver remains alive");
        }
        drop(sender);
        let receiver = Arc::new(Mutex::new(receiver));
        thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    let receiver = Arc::clone(&receiver);
                    scope.spawn(move || -> Result<(), ContentStoreError> {
                        loop {
                            let next = receiver.lock().expect("worker mutex poisoned").recv();
                            let Ok((id, bytes)) = next else {
                                return Ok(());
                            };
                            self.put_with_id(id, &bytes)?;
                        }
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().expect("content worker panicked")?;
            }
            Ok(())
        })
    }

    fn put_with_id(&self, id: ContentId, bytes: &[u8]) -> Result<(), ContentStoreError> {
        if ContentId::of(bytes) != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual: ContentId::of(bytes),
            });
        }
        if let Some(backend) = &self.backend {
            return backend.put_many(&[(id, bytes.to_vec())]);
        }
        let path = self.path(id);
        let parent = path.parent().expect("content path has parent");
        fs::create_dir_all(parent)?;

        // Persist both directory entries needed to reach a content file. Do this
        // even when the directories already exist: they may be leftovers from a
        // previous attempt whose directory sync failed, and must not become an
        // implicitly acknowledged success on retry.
        let mut directory = Some(nonempty_path(&self.root));
        while let Some(path) = directory {
            self.durability.sync_directory(path)?;
            directory = nonempty_parent(path);
        }

        if path.exists() {
            self.verify_existing(id, bytes)?;
            // The file may have been renamed by an attempt that subsequently
            // failed its parent-directory sync.
            self.durability.sync_directory(parent)?;
            return Ok(());
        }

        let temporary = parent.join(format!(".{}.{}.tmp", id, std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            self.durability.sync_file(&file)?;
            match fs::rename(&temporary, &path) {
                Ok(()) => {
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.verify_existing(id, bytes)?;
                    self.durability.sync_directory(parent)?;
                    Ok(())
                }
                Err(error) => Err(ContentStoreError::Io(error)),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn get(&self, id: ContentId) -> Result<Vec<u8>, ContentStoreError> {
        self.get_bounded(id, usize::MAX)
    }

    /// Reads and verifies content while refusing to allocate more than `max` bytes for it.
    ///
    /// Loose files are sized before allocation and an additional byte is probed after the
    /// declared length, so concurrent growth cannot turn the bound into an allocation hint.
    pub fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        if let Some(backend) = &self.backend {
            return backend.get_bounded(id, max);
        }
        let path = self.path(id);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(id));
            }
            Err(error) => return Err(ContentStoreError::Io(error)),
        };
        let size = file.metadata()?.len();
        if size > max as u64 {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: usize::try_from(size).unwrap_or(usize::MAX),
            });
        }
        let size = usize::try_from(size).map_err(|_| ContentStoreError::TooLarge {
            id,
            max,
            minimum: usize::MAX,
        })?;
        let bytes = read_exact_content(&mut file, id, size)?;
        let actual = ContentId::of(&bytes);
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        Ok(bytes)
    }

    #[must_use]
    pub fn contains(&self, id: ContentId) -> bool {
        if let Some(backend) = &self.backend {
            return backend.get_bounded(id, usize::MAX).is_ok();
        }
        self.path(id).is_file()
    }

    /// Fetch multiple content blobs, using concurrent reads when the
    /// backend supports it (S3). Falls back to sequential `get_bounded`
    /// for local filesystem backends.
    pub fn get_many_bounded(
        &self,
        ids: &[ContentId],
        max: usize,
    ) -> Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError> {
        if let Some(backend) = &self.backend
            && let Some(result) = backend.get_many_bounded(ids, max)
        {
            return result;
        }
        // Fallback: sequential reads (fast for local filesystem).
        let mut results = Vec::with_capacity(ids.len());
        for &id in ids {
            let bytes = self.get_bounded(id, max)?;
            results.push((id, bytes));
        }
        Ok(results)
    }

    /// Return the set of content IDs that already exist in this store,
    /// filtered to only those in `candidates`.
    ///
    /// When the backend supports listing (remote/S3), this does one
    /// LIST call and filters in memory — O(1) network round trips
    /// instead of O(n) per-item existence checks. Falls back to
    /// per-item `contains()` for local filesystem backends.
    pub fn existing_ids(
        &self,
        candidates: &[ContentId],
    ) -> Result<std::collections::HashSet<ContentId>, ContentStoreError> {
        if let Some(backend) = &self.backend
            && let Some(result) = backend.list_ids()
        {
            let all: std::collections::HashSet<ContentId> =
                result?.into_iter().collect();
            return Ok(candidates.iter().copied().filter(|id| all.contains(id)).collect());
        }
        // Fallback: per-item check (fast for local filesystem).
        Ok(candidates.iter().copied().filter(|id| self.contains(*id)).collect())
    }

    /// Loose fallback path, not a locator for packed content.
    #[must_use]
    pub fn path(&self, id: ContentId) -> PathBuf {
        let encoded = id.to_string();
        self.root.join(&encoded[..2]).join(&encoded[2..])
    }

    fn verify_existing(
        &self,
        id: ContentId,
        expected_bytes: &[u8],
    ) -> Result<(), ContentStoreError> {
        let actual_bytes = self.get(id)?;
        if actual_bytes != expected_bytes {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual: ContentId::of(&actual_bytes),
            });
        }
        Ok(())
    }
}

fn read_exact_content(
    reader: &mut impl io::Read,
    id: ContentId,
    size: usize,
) -> Result<Vec<u8>, ContentStoreError> {
    let mut bytes = vec![0; size];
    if let Err(error) = reader.read_exact(&mut bytes) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Err(ContentStoreError::Changed { id })
        } else {
            Err(ContentStoreError::Io(error))
        };
    }
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(ContentStoreError::Changed { id });
    }
    Ok(bytes)
}

fn nonempty_parent(path: &Path) -> Option<&Path> {
    if path == Path::new(".") {
        None
    } else {
        path.parent().map(nonempty_path)
    }
}

fn nonempty_path(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_content_reader_detects_shrink_and_growth() {
        let id = ContentId::of(b"expected");
        assert!(matches!(
            read_exact_content(&mut &b"short"[..], id, 8),
            Err(ContentStoreError::Changed { id: actual }) if actual == id
        ));
        assert!(matches!(
            read_exact_content(&mut &b"too long"[..], id, 3),
            Err(ContentStoreError::Changed { id: actual }) if actual == id
        ));
        assert_eq!(
            read_exact_content(&mut &b"exact"[..], id, 5).unwrap(),
            b"exact"
        );
    }
}
