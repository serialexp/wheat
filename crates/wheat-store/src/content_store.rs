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
