//! Local content-addressed storage for the Wheat prototype.
//!
//! Repository model identities remain in `wheat-core`; this crate owns only
//! byte retention and integrity verification for those identities.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use thiserror::Error;
use wheat_core::ContentId;

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
        let path = self.path(id);
        if path.exists() {
            self.verify_existing(id, bytes)?;
            return Ok(id);
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
        result?;
        Ok(id)
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
