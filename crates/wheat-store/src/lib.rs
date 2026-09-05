//! Local content-addressed storage for the Wheat prototype.
//!
//! Repository model identities remain in `wheat-core`; this crate owns only
//! byte retention and integrity verification for those identities.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use thiserror::Error;
use wheat_core::ContentId;

mod application_map;
mod loose;
mod merkle_tree;
mod object_store_adapter;
mod publication;
mod radix;
mod repository_hydration;
mod repository_publication;
mod repository_reader;
mod repository_transition;
mod repository_verify;
pub use repository_reader::RepositoryReader;
mod revision_map;
mod sets;

pub use application_map::{ApplicationMapCursor, ApplicationMapError, ApplicationMapStore};
pub use loose::{
    ImmutableObjectStore, LooseObjectStore, ObjectKind, ObjectStoreError, StoredObject,
};
pub use merkle_tree::{MerkleTreeError, MerkleTreeStore, TreeEdit};
pub use object_store_adapter::{
    ObjectStoreAdapter, ObjectStoreAdapterBuildError, S3ObjectStoreConfig,
};
pub use publication::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};
pub use repository_publication::{RepositoryPublicationError, RepositoryPublisher};
pub use revision_map::{RevisionMapCursor, RevisionMapError, RevisionMapStore};
pub use sets::{
    CollectionError, ConflictSetCursor, ConflictSetStore, ContentSetCursor, ContentSetStore,
    RetainedSetCursor, RetainedSetStore,
};

#[derive(Debug, Error)]
pub enum ContentStoreError {
    #[error("content {0} is missing")]
    Missing(ContentId),
    #[error("content {expected} failed integrity verification (found {actual})")]
    Corrupt {
        expected: ContentId,
        actual: ContentId,
    },
    #[error("content store I/O failed")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
}

impl ContentStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
            if let Some(previous) = deduplicated.insert(id, bytes.clone())
                && previous != bytes
            {
                return Err(ContentStoreError::Corrupt {
                    expected: id,
                    actual: ContentId::of(&bytes),
                });
            }
        }
        if deduplicated.is_empty() {
            return Ok(());
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
        let path = self.path(id);
        if path.exists() {
            self.verify_existing(id, bytes)?;
            return Ok(());
        }

        let parent = path.parent().expect("content path has parent");
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}.{}.tmp", id, std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            match fs::rename(&temporary, &path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.verify_existing(id, bytes)
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
        let path = self.path(id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(id));
            }
            Err(error) => return Err(ContentStoreError::Io(error)),
        };
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
        self.path(id).is_file()
    }

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
