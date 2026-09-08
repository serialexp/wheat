use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    path::PathBuf,
};

use thiserror::Error;
use wheat_core::RepositoryEnvelopeId;

use crate::Durability;

const HEAD_MAGIC: &[u8; 9] = b"WHEATHEAD";
const HEAD_VERSION: u8 = 2;
const ENCODED_HEAD_LEN: usize = HEAD_MAGIC.len() + 1 + 8 + 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryHead {
    pub generation: u64,
    pub envelope: RepositoryEnvelopeId,
}

impl RepositoryHead {
    #[must_use]
    pub fn initial(envelope: RepositoryEnvelopeId) -> Self {
        Self {
            generation: 0,
            envelope,
        }
    }

    pub fn next(self, envelope: RepositoryEnvelopeId) -> Result<Self, PublicationError> {
        Ok(Self {
            generation: self
                .generation
                .checked_add(1)
                .ok_or(PublicationError::InvalidGeneration)?,
            envelope,
        })
    }

    fn encode(self) -> [u8; ENCODED_HEAD_LEN] {
        let mut encoded = [0_u8; ENCODED_HEAD_LEN];
        encoded[..HEAD_MAGIC.len()].copy_from_slice(HEAD_MAGIC);
        encoded[HEAD_MAGIC.len()] = HEAD_VERSION;
        encoded[HEAD_MAGIC.len() + 1..HEAD_MAGIC.len() + 9]
            .copy_from_slice(&self.generation.to_be_bytes());
        encoded[HEAD_MAGIC.len() + 9..].copy_from_slice(self.envelope.as_bytes());
        encoded
    }

    fn decode(encoded: &[u8]) -> Result<Self, PublicationError> {
        if encoded.len() != ENCODED_HEAD_LEN
            || &encoded[..HEAD_MAGIC.len()] != HEAD_MAGIC
            || encoded[HEAD_MAGIC.len()] != HEAD_VERSION
        {
            return Err(PublicationError::CorruptHead);
        }
        let generation = u64::from_be_bytes(
            encoded[HEAD_MAGIC.len() + 1..HEAD_MAGIC.len() + 9]
                .try_into()
                .expect("fixed generation range"),
        );
        let envelope = RepositoryEnvelopeId::from_digest(
            encoded[HEAD_MAGIC.len() + 9..]
                .try_into()
                .expect("fixed digest range"),
        );
        Ok(Self {
            generation,
            envelope,
        })
    }
}

#[derive(Debug, Error)]
pub enum PublicationError {
    #[error("repository head is not initialized")]
    Missing,
    #[error("repository head is corrupt or has an unsupported version")]
    CorruptHead,
    #[error("publication expected {expected:?}, but current head is {actual:?}")]
    Stale {
        expected: RepositoryHead,
        actual: RepositoryHead,
    },
    #[error("proposed generation must be exactly one greater than the current generation")]
    InvalidGeneration,
    #[error("publication store I/O failed")]
    Io(#[from] io::Error),
}

pub trait PublicationStore {
    fn read(&self) -> Result<RepositoryHead, PublicationError>;

    fn initialize(&self, initial: RepositoryHead) -> Result<(), PublicationError>;

    fn compare_and_advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
    ) -> Result<(), PublicationError>;
}

#[derive(Clone, Debug)]
pub struct LocalPublicationStore {
    head_path: PathBuf,
    lock_path: PathBuf,
    durability: Durability,
}

impl LocalPublicationStore {
    #[must_use]
    pub fn new(head_path: impl Into<PathBuf>) -> Self {
        Self::with_durability(head_path, Durability::Durable)
    }

    #[must_use]
    pub fn with_durability(head_path: impl Into<PathBuf>, durability: Durability) -> Self {
        let head_path = head_path.into();
        let lock_path = head_path.with_extension("lock");
        Self {
            head_path,
            lock_path,
            durability,
        }
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, PublicationError>,
    ) -> Result<T, PublicationError> {
        let parent = self.head_path.parent().ok_or_else(|| {
            PublicationError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "head path has no parent",
            ))
        })?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock_exclusive(&lock)?;
        let result = operation();
        let unlock_result = unlock(&lock);
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(PublicationError::Io(error)),
        }
    }

    fn read_unlocked(&self) -> Result<RepositoryHead, PublicationError> {
        let bytes = match fs::read(&self.head_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PublicationError::Missing);
            }
            Err(error) => return Err(PublicationError::Io(error)),
        };
        RepositoryHead::decode(&bytes)
    }

    fn write_unlocked(&self, head: RepositoryHead) -> Result<(), PublicationError> {
        let parent = self.head_path.parent().expect("validated head parent");
        let temporary = self.head_path.with_extension(format!(
            "head.{}.{}.tmp",
            std::process::id(),
            head.generation
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&head.encode())?;
            self.durability.sync_file(&file)?;
            fs::rename(&temporary, &self.head_path)?;
            self.durability.sync_directory(parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl PublicationStore for LocalPublicationStore {
    fn read(&self) -> Result<RepositoryHead, PublicationError> {
        self.with_lock(|| self.read_unlocked())
    }

    fn initialize(&self, initial: RepositoryHead) -> Result<(), PublicationError> {
        self.with_lock(|| match self.read_unlocked() {
            Err(PublicationError::Missing) => self.write_unlocked(initial),
            Ok(actual) if actual == initial => Ok(()),
            Ok(actual) => Err(PublicationError::Stale {
                expected: initial,
                actual,
            }),
            Err(error) => Err(error),
        })
    }

    fn compare_and_advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
    ) -> Result<(), PublicationError> {
        let Some(next_generation) = expected.generation.checked_add(1) else {
            return Err(PublicationError::InvalidGeneration);
        };
        if proposed.generation != next_generation {
            return Err(PublicationError::InvalidGeneration);
        }
        self.with_lock(|| {
            let actual = self.read_unlocked()?;
            if actual != expected {
                return Err(PublicationError::Stale { expected, actual });
            }
            self.write_unlocked(proposed)
        })
    }
}

fn lock_exclusive(file: &File) -> Result<(), io::Error> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlock(file: &File) -> Result<(), io::Error> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
